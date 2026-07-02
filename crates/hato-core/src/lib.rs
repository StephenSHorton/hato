//! Hato core — peer-to-peer file transfer over [iroh].
//!
//! A sender imports a file (or folder) into an on-disk blob store and serves it;
//! a [`BlobTicket`] encodes how to reach the sender and what to fetch. A receiver
//! connects with that ticket, downloads the content, and exports it to disk.
//!
//! Built on iroh 1.0 + iroh-blobs 0.103 (the stack behind n0's `sendme`).

use std::path::{Path, PathBuf};

use anyhow::Context;
use n0_future::StreamExt;

use iroh::{endpoint::presets, protocol::Router, Endpoint};
use iroh_blobs::{
    api::{
        blobs::{
            AddPathOptions, AddProgressItem, ExportMode, ExportOptions, ExportProgressItem,
            ImportMode,
        },
        remote::GetProgressItem,
        Store, TempTag,
    },
    format::collection::Collection,
    store::fs::FsStore,
    BlobFormat, BlobsProtocol,
};
use walkdir::WalkDir;

/// Re-exported so callers (the CLI) can parse a ticket as a `clap` argument
/// without depending on `iroh-blobs` directly.
pub use iroh_blobs::ticket::BlobTicket;

/// A prepared outgoing transfer.
///
/// The file(s) have been imported into an on-disk store and the blobs protocol
/// is being served. Keep this alive to keep serving; [`Outgoing::ticket`] is what
/// the receiver needs. The held temp-tags stop the imported data being GC'd.
pub struct Outgoing {
    router: Router,
    store: FsStore,
    ticket: BlobTicket,
    _root_tag: TempTag,
    _file_tags: Vec<TempTag>,
}

impl Outgoing {
    /// The ticket to hand to the receiver.
    pub fn ticket(&self) -> &BlobTicket {
        &self.ticket
    }

    /// Stop serving and flush the on-disk store cleanly.
    pub async fn shutdown(self) -> anyhow::Result<()> {
        self.router.shutdown().await?;
        self.store.shutdown().await?;
        Ok(())
    }
}

/// Import `path` (a file or folder) into a store at `store_dir` and start serving.
///
/// Returns an [`Outgoing`] holding the ticket. The source file is *referenced in
/// place* (not copied into the store), so multi-gigabyte sends are cheap — don't
/// move or delete `path` while the returned [`Outgoing`] is still serving.
pub async fn prepare_send(path: &Path, store_dir: &Path) -> anyhow::Result<Outgoing> {
    let endpoint = Endpoint::builder(presets::N0)
        .alpns(vec![iroh_blobs::ALPN.to_vec()])
        .bind()
        .await?;

    let store = FsStore::load(store_dir).await?;

    // Import into a Collection (a single file becomes a one-entry collection),
    // then store the hash-seq root and mint a ticket for it.
    let (collection, file_tags) = import(path, store.as_ref()).await?;
    let root_tag = collection.store(store.as_ref()).await?;
    let hash = root_tag.hash();

    let blobs = BlobsProtocol::new(&store, None);
    let router = Router::builder(endpoint)
        .accept(iroh_blobs::ALPN, blobs)
        .spawn();

    // Wait until we have reachable addresses before minting the ticket, so the
    // receiver gets our relay URL + direct addrs baked in.
    {
        let ep = router.endpoint();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(30), ep.online()).await;
    }

    let addr = router.endpoint().addr();
    let ticket = BlobTicket::new(addr, hash, BlobFormat::HashSeq);

    Ok(Outgoing {
        router,
        store,
        ticket,
        _root_tag: root_tag,
        _file_tags: file_tags,
    })
}

/// Connect using `ticket`, download into a scratch store at `store_dir`, and
/// export the file(s) into `outdir`. `on_progress` is called with the cumulative
/// number of bytes received. Returns the total bytes transferred.
pub async fn receive(
    ticket: &BlobTicket,
    outdir: &Path,
    store_dir: &Path,
    mut on_progress: impl FnMut(u64),
) -> anyhow::Result<u64> {
    let endpoint = Endpoint::builder(presets::N0).bind().await?;
    let store = FsStore::load(store_dir).await?;

    let addr = ticket.addr().clone();
    let haf = ticket.hash_and_format();

    // Connect and download everything missing (single blob or whole hash-seq).
    let conn = endpoint.connect(addr, iroh_blobs::ALPN).await?;
    let mut stream = store.remote().fetch(conn, haf).stream();
    let mut total = 0u64;
    while let Some(item) = stream.next().await {
        match item {
            GetProgressItem::Progress(offset) => {
                total = offset;
                on_progress(offset);
            }
            GetProgressItem::Done(_stats) => break,
            GetProgressItem::Error(e) => anyhow::bail!("download failed: {e}"),
        }
    }

    // Rebuild filenames from the collection and write them to disk.
    let collection = Collection::load(haf.hash, store.as_ref()).await?;
    export(store.as_ref(), collection, outdir).await?;

    endpoint.close().await;
    store.shutdown().await?;
    Ok(total)
}

/// Walk `path` and import every file into `db` as a [`Collection`].
async fn import(path: &Path, db: &Store) -> anyhow::Result<(Collection, Vec<TempTag>)> {
    // Canonicalize first: on Windows this yields a `\\?\` verbatim path, which is
    // long-path safe.
    let path = path.canonicalize()?;
    let root = if path.is_dir() {
        path.as_path()
    } else {
        path.parent().context("path has no parent")?
    };

    let mut names_and_tags: Vec<(String, TempTag)> = Vec::new();
    for entry in WalkDir::new(&path) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let file_path: PathBuf = entry.into_path();

        // Blob names must use '/' separators; convert Windows '\'.
        let rel = file_path.strip_prefix(root).unwrap_or(&file_path);
        let name = rel.to_string_lossy().replace('\\', "/");

        let mut stream = db
            .add_path_with_opts(AddPathOptions {
                path: file_path,
                mode: ImportMode::TryReference, // reference source; don't copy multi-GB
                format: BlobFormat::Raw,
            })
            .stream()
            .await;
        let tag = loop {
            match stream
                .next()
                .await
                .context("add stream ended before completing")?
            {
                AddProgressItem::Done(tt) => break tt,
                AddProgressItem::Error(e) => anyhow::bail!("import failed for {name}: {e}"),
                _ => {}
            }
        };
        names_and_tags.push((name, tag));
    }

    anyhow::ensure!(
        !names_and_tags.is_empty(),
        "nothing to send: {} contained no files",
        path.display()
    );

    names_and_tags.sort_by(|(a, _), (b, _)| a.cmp(b));
    let (collection, tags) = names_and_tags
        .into_iter()
        .map(|(name, tag)| ((name, tag.hash()), tag))
        .unzip::<_, _, Collection, Vec<_>>();
    Ok((collection, tags))
}

/// Export every entry of `collection` from `db` into `outdir`, preserving names.
async fn export(db: &Store, collection: Collection, outdir: &Path) -> anyhow::Result<()> {
    for (name, hash) in collection.iter() {
        let mut target = outdir.to_path_buf();
        for part in name.split('/') {
            target.push(part);
        }
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut stream = db
            .export_with_opts(ExportOptions {
                hash: *hash,
                target,
                mode: ExportMode::Copy, // Copy is safest across volumes on Windows
            })
            .stream()
            .await;
        while let Some(item) = stream.next().await {
            match item {
                ExportProgressItem::Error(e) => anyhow::bail!("export failed for {name}: {e}"),
                ExportProgressItem::Done => {}
                _ => {}
            }
        }
    }
    Ok(())
}
