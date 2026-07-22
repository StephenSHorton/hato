//! Hato core — peer-to-peer file transfer over [iroh].
//!
//! A sender imports a file (or folder) into an on-disk blob store and serves it;
//! a [`BlobTicket`] encodes how to reach the sender and what to fetch. A receiver
//! connects with that ticket, downloads the content, and exports it to disk.
//!
//! Built on iroh 1.0 + iroh-blobs 0.103 (the stack behind n0's `sendme`).
//!
//! Phase 4 adds a **persistent identity** and **contacts** so paired machines can
//! offer tickets directly (`offer`) without pasting codes.

pub mod contacts;
pub mod identity;
pub mod offer;

use std::path::{Path, PathBuf};

use anyhow::Context;
use n0_future::StreamExt;

use iroh::{endpoint::presets, protocol::Router, TransportAddr};
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
    get::request::get_hash_seq_and_sizes,
    store::fs::FsStore,
    BlobFormat, BlobsProtocol,
};
use walkdir::WalkDir;

/// Re-export iroh identity types used by contacts / offers.
pub use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
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

    /// The live endpoint serving this transfer (for dialing contacts, etc.).
    pub fn endpoint(&self) -> &Endpoint {
        self.router.endpoint()
    }

    /// This machine's endpoint id for the transfer.
    pub fn endpoint_id(&self) -> EndpointId {
        self.router.endpoint().id()
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
///
/// When `secret_key` is `Some`, the endpoint uses that identity (stable
/// [`EndpointId`] for contacts). When `None`, a fresh ephemeral key is generated.
pub async fn prepare_send(
    path: &Path,
    store_dir: &Path,
    relay_only: bool,
    secret_key: Option<SecretKey>,
) -> anyhow::Result<Outgoing> {
    let mut builder = Endpoint::builder(presets::N0).alpns(vec![iroh_blobs::ALPN.to_vec()]);
    if let Some(sk) = secret_key {
        builder = builder.secret_key(sk);
    }
    let endpoint = builder.bind().await?;

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

    let mut addr = router.endpoint().addr();
    if relay_only {
        // Drop direct IP addresses, keeping only the relay URL. The receiver then
        // has no way to dial us directly and must connect through iroh's relay —
        // exactly the path used between two machines that can't reach each other.
        addr.addrs = addr
            .addrs
            .iter()
            .filter(|a| matches!(a, TransportAddr::Relay(_)))
            .cloned()
            .collect();
    }
    let ticket = BlobTicket::new(addr, hash, BlobFormat::HashSeq);

    Ok(Outgoing {
        router,
        store,
        ticket,
        _root_tag: root_tag,
        _file_tags: file_tags,
    })
}

/// Convenience: [`prepare_send`] using the machine's persistent identity.
pub async fn prepare_send_identified(
    path: &Path,
    store_dir: &Path,
    relay_only: bool,
) -> anyhow::Result<Outgoing> {
    let sk = identity::load_or_create_secret_key()?;
    prepare_send(path, store_dir, relay_only, Some(sk)).await
}

/// The outcome of a [`receive`] call.
pub struct RecvSummary {
    /// Total size of the transfer, in bytes.
    pub total_bytes: u64,
    /// Bytes already present in the store before this run. Non-zero means the
    /// transfer *resumed* an earlier interrupted download.
    pub already_had: u64,
}

/// Connect using `ticket`, download into a store at `store_dir`, and export the
/// file(s) into `outdir`. `on_progress` is called with `(bytes_done, bytes_total)`.
///
/// Resumes automatically: if `store_dir` already holds part of this transfer
/// (from an interrupted run), only the missing byte ranges are fetched. Point a
/// re-run at the same `store_dir` and it picks up where it left off.
///
/// When `secret_key` is provided, the receiver uses that identity (useful so
/// the peer can recognize us later); otherwise ephemeral.
pub async fn receive(
    ticket: &BlobTicket,
    outdir: &Path,
    store_dir: &Path,
    on_progress: impl FnMut(u64, u64),
) -> anyhow::Result<RecvSummary> {
    receive_with_key(ticket, outdir, store_dir, None, on_progress).await
}

/// Like [`receive`], but with an optional persistent identity.
pub async fn receive_with_key(
    ticket: &BlobTicket,
    outdir: &Path,
    store_dir: &Path,
    secret_key: Option<SecretKey>,
    mut on_progress: impl FnMut(u64, u64),
) -> anyhow::Result<RecvSummary> {
    let mut builder = Endpoint::builder(presets::N0);
    if let Some(sk) = secret_key {
        builder = builder.secret_key(sk);
    }
    let endpoint = builder.bind().await?;
    let store = FsStore::load(store_dir).await?;

    let haf = ticket.hash_and_format();
    let addr = ticket.addr().clone();

    // Resume: inspect what this store already holds for this hash.
    let local = store.remote().local(haf).await?;
    let already_had = local.local_bytes();
    let complete = local.is_complete();

    let total_bytes = if complete {
        already_had
    } else {
        let conn = endpoint.connect(addr, iroh_blobs::ALPN).await?;

        // Ask the sender for every blob's size so we can show a real % / ETA bar.
        let (_hash_seq, sizes) =
            get_hash_seq_and_sizes(&conn, &haf.hash, 1024 * 1024 * 32, None).await?;
        let total: u64 = sizes.iter().copied().sum();
        on_progress(already_had, total);

        // Fetch only the missing ranges — this is what makes resume work.
        let get = store.remote().execute_get(conn, local.missing());
        let mut stream = get.stream();
        while let Some(item) = stream.next().await {
            match item {
                GetProgressItem::Progress(offset) => on_progress(already_had + offset, total),
                GetProgressItem::Done(_stats) => break,
                GetProgressItem::Error(e) => anyhow::bail!("download failed: {e}"),
            }
        }
        total
    };

    // Rebuild filenames from the collection and write them to disk.
    let collection = Collection::load(haf.hash, store.as_ref()).await?;
    export(store.as_ref(), collection, outdir).await?;

    endpoint.close().await;
    store.shutdown().await?;
    Ok(RecvSummary {
        total_bytes,
        already_had,
    })
}

/// Summarize a ticket's address as `(relay_url_count, direct_ip_count)`.
///
/// A `(1, 0)` ticket is relay-only: the receiver has no direct address to dial
/// and must reach the sender through iroh's relay servers.
pub fn ticket_addr_summary(ticket: &BlobTicket) -> (usize, usize) {
    let addr = ticket.addr();
    (addr.relay_urls().count(), addr.ip_addrs().count())
}

/// Serialize a [`BlobTicket`] to its canonical string form as bytes.
///
/// This is the glue the short-code layer (`hato-code`) needs: it works purely in
/// bytes and never depends on `iroh`/`iroh-blobs`, so the CLI converts here. The
/// round-trip is `ticket_from_bytes(ticket_to_bytes(t)) == t`.
pub fn ticket_to_bytes(ticket: &BlobTicket) -> Vec<u8> {
    ticket.to_string().into_bytes()
}

/// Parse a [`BlobTicket`] back from the bytes produced by [`ticket_to_bytes`].
///
/// Errors if the bytes are not valid UTF-8 or not a well-formed ticket.
pub fn ticket_from_bytes(bytes: &[u8]) -> anyhow::Result<BlobTicket> {
    let s = std::str::from_utf8(bytes).context("ticket bytes are not valid UTF-8")?;
    s.parse::<BlobTicket>()
        .map_err(|e| anyhow::anyhow!("invalid ticket: {e}"))
}

/// Convert a relative path into a blob name, which must always use `/` as the
/// separator (even on Windows, where paths use `\`).
fn to_blob_name(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
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
        let name = to_blob_name(rel);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_names_are_forward_slashed() {
        assert_eq!(to_blob_name(Path::new("a\\b\\c.txt")), "a/b/c.txt");
        assert_eq!(
            to_blob_name(Path::new("sub/dir/file.bin")),
            "sub/dir/file.bin"
        );
        assert_eq!(to_blob_name(Path::new("solo.zip")), "solo.zip");
    }

    #[test]
    fn ticket_from_bytes_rejects_garbage() {
        // Non-UTF-8 and non-ticket strings must both error. The happy-path
        // round-trip (a real ticket -> bytes -> ticket -> download) is exercised
        // end-to-end by the hato-code + CLI e2e, using a genuine ticket.
        assert!(ticket_from_bytes(&[0xff, 0xfe, 0xfd]).is_err());
        assert!(ticket_from_bytes(b"not-a-ticket").is_err());
        assert!(ticket_from_bytes(b"").is_err());
    }
}
