//! OMP CLI. Every command wraps one `omp_core::api::Repo` call.
//!
//! Default transport is in-process; `--remote <url>` turns the CLI into a
//! thin HTTP client (deferred — v1 ships with the in-process transport only).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use omp_core::api::{AuthorOverride, Fields, Repo};
use omp_core::keys::TenantKeys;
use omp_core::manifest::FieldValue;
use omp_core::registry::{default_registry_path, Quotas, TenantRegistry};
use omp_core::tenant::TenantId;

#[derive(Parser)]
#[command(name = "omp", version, about = "OpenMemoryProtocol CLI")]
struct Cli {
    /// Optional repo root (defaults to cwd).
    #[arg(long, global = true)]
    repo: Option<PathBuf>,
    /// Talk to an HTTP server instead of the local repo (v1 stub).
    #[arg(long, global = true)]
    remote: Option<String>,
    /// Include provenance hashes (source_hash, schema_hash, probe_hashes,
    /// manifest_hash, tree/parents) in JSON output. Default off — hashes
    /// are noise for day-to-day browsing; useful for replay and audit.
    #[arg(short = 'v', long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize a new repo.
    Init,
    /// Show staged changes, current branch, HEAD.
    Status,
    /// Stage a file. Path is the repo-relative destination; bytes come from disk.
    Add {
        /// Repo-relative path.
        path: String,
        /// Source file to read from disk. If omitted, `path` is read from the
        /// repo's working tree at that location.
        #[arg(long = "from")]
        from: Option<PathBuf>,
        /// `key=value` pairs for user-provided fields.
        #[arg(long = "field", value_parser = parse_field_kv)]
        field: Vec<(String, FieldValue)>,
        /// Override file-type detection.
        #[arg(long = "type")]
        file_type: Option<String>,
    },
    /// Show a manifest / blob / tree.
    Show {
        path: String,
        #[arg(long = "at")]
        at: Option<String>,
    },
    /// Dump the file bytes (alias: `cat`).
    Cat {
        path: String,
        #[arg(long = "at")]
        at: Option<String>,
    },
    /// List a directory (or the root tree).
    Ls {
        #[arg(default_value = "")]
        path: String,
        #[arg(long = "at")]
        at: Option<String>,
        #[arg(long = "recursive")]
        recursive: bool,
    },
    /// Update user-provided fields on an existing manifest.
    #[command(name = "patch-fields")]
    PatchFields {
        path: String,
        #[arg(long = "field", value_parser = parse_field_kv)]
        field: Vec<(String, FieldValue)>,
    },
    /// Stage a deletion.
    Rm { path: String },
    /// Commit staged changes.
    Commit {
        #[arg(short = 'm', long)]
        message: String,
        #[arg(long)]
        author: Option<String>,
    },
    /// Print commit history.
    Log {
        #[arg(long, default_value_t = 50)]
        max: usize,
        path: Option<String>,
    },
    /// Print a structured diff between two refs.
    Diff {
        from: String,
        to: String,
        path: Option<String>,
    },
    /// Create a branch (no args: list them).
    Branch {
        name: Option<String>,
        start: Option<String>,
    },
    /// Switch HEAD.
    Checkout {
        #[arg(name = "ref")]
        ref_: String,
    },
    /// Dry-run an ingest (no staging).
    #[command(name = "test-ingest")]
    TestIngest {
        path: String,
        #[arg(long = "from")]
        from: Option<PathBuf>,
        #[arg(long = "field", value_parser = parse_field_kv)]
        field: Vec<(String, FieldValue)>,
        #[arg(long = "proposed-schema")]
        proposed_schema: Option<PathBuf>,
    },
    /// Run the HTTP server (v1: delegates to the omp-server binary).
    Serve {
        #[arg(long)]
        bind: Option<String>,
    },
    /// Administrative commands (tenant management, etc).
    #[command(subcommand)]
    Admin(AdminCommand),
    /// End-to-end-encrypted operations. See doc 13.
    #[command(subcommand)]
    Enc(EncCommand),
    /// Cryptographic sharing primitive (doc 13 §Sharing).
    #[command(subcommand)]
    Share(ShareCommand),
}

#[derive(Subcommand)]
enum EncCommand {
    /// Commit staged changes with tree-name + commit-message encryption.
    Commit {
        #[arg(short = 'm', long)]
        message: String,
        #[arg(long)]
        author: Option<String>,
        #[arg(long, default_value = "_local")]
        tenant: String,
        #[arg(long)]
        passphrase: Option<String>,
    },
    /// Initialize an encrypted repo. Derives keys from the passphrase,
    /// generates a fresh X25519 identity, and persists the wrapped
    /// private half at `.omp/encrypted-identity`.
    Init {
        /// Tenant id used as the KDF salt. Single-tenant local dev can
        /// leave this as `_local` (the default).
        #[arg(long, default_value = "_local")]
        tenant: String,
        /// Passphrase. Read from env `OMP_PASSPHRASE` if unset.
        #[arg(long)]
        passphrase: Option<String>,
    },
    /// Encrypted ingest: like `omp add`, but seals the file client-side.
    Add {
        path: String,
        #[arg(long = "from")]
        from: Option<PathBuf>,
        #[arg(long = "field", value_parser = parse_field_kv)]
        field: Vec<(String, FieldValue)>,
        #[arg(long = "type")]
        file_type: Option<String>,
        #[arg(long, default_value = "_local")]
        tenant: String,
        #[arg(long)]
        passphrase: Option<String>,
    },
    /// Decrypt and print a file's plaintext bytes to stdout.
    Show {
        path: String,
        #[arg(long = "at")]
        at: Option<String>,
        #[arg(long, default_value = "_local")]
        tenant: String,
        #[arg(long)]
        passphrase: Option<String>,
    },
}

#[derive(Subcommand)]
enum ShareCommand {
    /// Emit a `share` object granting access to `path` to one or more
    /// recipient X25519 public keys (hex).
    Grant {
        path: String,
        /// Recipient spec: `tenant:hex_pubkey`. Repeatable.
        #[arg(long = "to", required = true)]
        to: Vec<String>,
        #[arg(long, default_value = "_local")]
        tenant: String,
        #[arg(long)]
        passphrase: Option<String>,
    },
    /// Revoke by rewriting the underlying source under a fresh content
    /// key and emitting a new share without the revoked recipients.
    Revoke {
        path: String,
        /// The recipients to keep (same `tenant:hex_pubkey` spec). Any
        /// tenant not listed here is revoked.
        #[arg(long = "keep")]
        keep: Vec<String>,
        #[arg(long, default_value = "_local")]
        tenant: String,
        #[arg(long)]
        passphrase: Option<String>,
    },
    /// Print this tenant's X25519 public key (hex). Share it with a
    /// collaborator so they can `omp share grant --to you:<hex>`.
    Pubkey {
        #[arg(long, default_value = "_local")]
        tenant: String,
        #[arg(long)]
        passphrase: Option<String>,
    },
}

#[derive(Subcommand)]
enum AdminCommand {
    /// Tenant-registry management.
    #[command(subcommand)]
    Tenant(TenantCommand),
}

#[derive(Subcommand)]
enum TenantCommand {
    /// Create a new tenant. Prints the generated Bearer token (shown once).
    Create {
        /// Tenant id, e.g. `alice`.
        name: String,
        /// Path to `tenants.toml`. Defaults to `<tenants-base>/admin/tenants.toml`.
        #[arg(long)]
        registry: Option<PathBuf>,
        /// Tenants-base directory; used to derive the default registry path.
        #[arg(long = "tenants-base")]
        tenants_base: Option<PathBuf>,
        /// Optional byte cap.
        #[arg(long)]
        bytes: Option<u64>,
        /// Optional object-count cap.
        #[arg(long)]
        object_count: Option<u64>,
        /// Optional per-request probe fuel cap.
        #[arg(long = "probe-fuel")]
        probe_fuel: Option<u64>,
        /// Optional per-request wall-clock cap (seconds).
        #[arg(long = "wall-clock-s")]
        wall_clock_s: Option<u32>,
    },
    /// List every tenant in the registry.
    List {
        #[arg(long)]
        registry: Option<PathBuf>,
        #[arg(long = "tenants-base")]
        tenants_base: Option<PathBuf>,
    },
    /// Remove a tenant from the registry. On-disk files are left alone.
    Delete {
        name: String,
        #[arg(long)]
        registry: Option<PathBuf>,
        #[arg(long = "tenants-base")]
        tenants_base: Option<PathBuf>,
    },
    /// Update a tenant's quotas.
    SetQuota {
        name: String,
        #[arg(long)]
        registry: Option<PathBuf>,
        #[arg(long = "tenants-base")]
        tenants_base: Option<PathBuf>,
        #[arg(long)]
        bytes: Option<u64>,
        #[arg(long)]
        object_count: Option<u64>,
        #[arg(long = "probe-fuel")]
        probe_fuel: Option<u64>,
        #[arg(long = "wall-clock-s")]
        wall_clock_s: Option<u32>,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    if cli.remote.is_some() {
        bail!("--remote transport is not shipped in v1");
    }
    let repo_root = cli
        .repo
        .clone()
        .unwrap_or(std::env::current_dir().context("cwd")?);

    match cli.command {
        Command::Init => {
            Repo::init(&repo_root)?;
            println!("initialized OMP repo at {}", repo_root.display());
        }
        Command::Status => {
            let repo = Repo::open(&repo_root)?;
            let status = repo.status()?;
            println!("branch: {}", status.branch.unwrap_or_else(|| "(none)".into()));
            println!(
                "HEAD:   {}",
                status
                    .head
                    .map(|h| h.short())
                    .unwrap_or_else(|| "(none)".into())
            );
            if status.staged.is_empty() {
                println!("staged: (none)");
            } else {
                println!("staged:");
                for s in status.staged {
                    let mark = match s.kind {
                        omp_core::api::StagedKind::Upsert => "+",
                        omp_core::api::StagedKind::Delete => "-",
                    };
                    println!("  {mark} {}", s.path);
                }
            }
        }
        Command::Add {
            path,
            from,
            field,
            file_type,
        } => {
            let repo = Repo::open(&repo_root)?;
            let disk_path = from.unwrap_or_else(|| repo_root.join(&path));
            let bytes = std::fs::read(&disk_path)
                .with_context(|| format!("reading {}", disk_path.display()))?;
            let mut fields: Fields = BTreeMap::new();
            for (k, v) in field {
                fields.insert(k, v);
            }
            let res = repo.add(&path, &bytes, Some(fields), file_type.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&res)?);
        }
        Command::Show { path, at } => {
            let repo = Repo::open(&repo_root)?;
            let res = repo.show(&path, at.as_deref())?;
            print_show_result(&res, cli.verbose)?;
        }
        Command::Cat { path, at } => {
            let repo = Repo::open(&repo_root)?;
            let bytes = repo.bytes_of(&path, at.as_deref())?;
            use std::io::Write as _;
            std::io::stdout().write_all(&bytes)?;
        }
        Command::Ls { path, at, recursive } => {
            let repo = Repo::open(&repo_root)?;
            let entries = repo.ls(&path, at.as_deref(), recursive)?;
            for e in entries {
                println!("{:<9} {} {}", e.mode, e.hash.short(), e.name);
            }
        }
        Command::PatchFields { path, field } => {
            let repo = Repo::open(&repo_root)?;
            let mut fields: Fields = BTreeMap::new();
            for (k, v) in field {
                fields.insert(k, v);
            }
            let m = repo.patch_fields(&path, fields)?;
            print_manifest(&m, cli.verbose)?;
        }
        Command::Rm { path } => {
            let repo = Repo::open(&repo_root)?;
            repo.remove(&path)?;
            println!("staged removal: {path}");
        }
        Command::Commit { message, author } => {
            let repo = Repo::open(&repo_root)?;
            let override_ = author.map(|s| {
                // Parse `Name <email>` if possible, else use as name.
                if let Some((name, rest)) = s.split_once('<') {
                    if let Some(email) = rest.strip_suffix('>') {
                        return AuthorOverride {
                            name: Some(name.trim().to_string()),
                            email: Some(email.trim().to_string()),
                            timestamp: None,
                        };
                    }
                }
                AuthorOverride {
                    name: Some(s),
                    email: None,
                    timestamp: None,
                }
            });
            let h = repo.commit(&message, override_)?;
            println!("{}", h.hex());
        }
        Command::Log { max, path } => {
            let repo = Repo::open(&repo_root)?;
            let log = repo.log_commits(path.as_deref(), max)?;
            for c in log {
                let msg_first_line = c.message.lines().next().unwrap_or("");
                println!(
                    "{} {} <{}> {} {}",
                    c.hash.short(),
                    c.author,
                    c.email,
                    c.timestamp,
                    msg_first_line
                );
            }
        }
        Command::Diff { from, to, path } => {
            let repo = Repo::open(&repo_root)?;
            let diff = repo.diff(&from, &to, path.as_deref())?;
            for entry in diff {
                let marker = match entry.status {
                    omp_core::api::DiffStatus::Added => "A",
                    omp_core::api::DiffStatus::Removed => "D",
                    omp_core::api::DiffStatus::Modified => "M",
                    omp_core::api::DiffStatus::Unchanged => " ",
                };
                println!("{marker}  {}", entry.path);
            }
        }
        Command::Branch { name, start } => {
            let repo = Repo::open(&repo_root)?;
            match name {
                Some(n) => {
                    repo.branch(&n, start.as_deref())?;
                    println!("created branch {n}");
                }
                None => {
                    for b in repo.list_branches()? {
                        let prefix = if b.is_current { "*" } else { " " };
                        println!(
                            "{prefix} {:<20} {}",
                            b.name,
                            b.head.map(|h| h.short()).unwrap_or_else(|| "(none)".into())
                        );
                    }
                }
            }
        }
        Command::Checkout { ref_ } => {
            let repo = Repo::open(&repo_root)?;
            repo.checkout(&ref_)?;
            println!("checked out {ref_}");
        }
        Command::TestIngest {
            path,
            from,
            field,
            proposed_schema,
        } => {
            let repo = Repo::open(&repo_root)?;
            let disk_path = from.unwrap_or_else(|| repo_root.join(&path));
            let bytes = std::fs::read(&disk_path)
                .with_context(|| format!("reading {}", disk_path.display()))?;
            let mut fields: Fields = BTreeMap::new();
            for (k, v) in field {
                fields.insert(k, v);
            }
            let proposed = match proposed_schema {
                Some(p) => Some(std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?),
                None => None,
            };
            let m = repo.test_ingest(&path, &bytes, Some(fields), proposed.as_deref())?;
            print_manifest(&m, cli.verbose)?;
        }
        Command::Serve { bind } => {
            // Delegate to the sibling binary via PATH. The CLI crate doesn't
            // pull axum; the server is a separate binary (see docs/design/08).
            bail!(
                "v1 CLI does not embed the HTTP server; run `omp-server` instead{}",
                bind.map(|b| format!(" (with OMP_SERVER_BIND={b})"))
                    .unwrap_or_default()
            );
        }
        Command::Admin(admin) => handle_admin(admin)?,
        Command::Enc(cmd) => handle_enc(&repo_root, cmd)?,
        Command::Share(cmd) => handle_share(&repo_root, cmd)?,
    }
    Ok(())
}

// ---- Encrypted-mode CLI (docs/design/13-end-to-end-encryption.md) ----

const IDENTITY_FILENAME: &str = ".omp/encrypted-identity";

fn identity_path(repo_root: &Path) -> PathBuf {
    repo_root.join(IDENTITY_FILENAME)
}

fn get_passphrase(arg: Option<String>) -> Result<Vec<u8>> {
    if let Some(p) = arg {
        return Ok(p.into_bytes());
    }
    if let Ok(p) = std::env::var("OMP_PASSPHRASE") {
        return Ok(p.into_bytes());
    }
    bail!("no passphrase supplied (--passphrase or OMP_PASSPHRASE)")
}

/// Unlock keys and attach the identity from the on-disk slot.
fn unlock_with_identity(
    repo_root: &Path,
    tenant_id: &str,
    passphrase: Option<String>,
) -> Result<TenantKeys> {
    let tenant = TenantId::new(tenant_id)
        .map_err(|e| anyhow::anyhow!("invalid tenant id: {e}"))?;
    let pass = get_passphrase(passphrase)?;
    let mut keys = TenantKeys::unlock(&pass, &tenant)
        .map_err(|e| anyhow::anyhow!("unlock: {e}"))?;
    let id_path = identity_path(repo_root);
    if id_path.exists() {
        let sealed = std::fs::read(&id_path)
            .with_context(|| format!("read {}", id_path.display()))?;
        keys.unseal_and_attach_identity(&sealed)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    Ok(keys)
}

fn handle_enc(repo_root: &Path, cmd: EncCommand) -> Result<()> {
    match cmd {
        EncCommand::Init { tenant, passphrase } => {
            // Init the repo skeleton if needed.
            if !repo_root.join(".omp").exists() {
                Repo::init(repo_root)?;
            }
            let tenant_id = TenantId::new(&tenant)
                .map_err(|e| anyhow::anyhow!("invalid tenant id: {e}"))?;
            let pass = get_passphrase(passphrase)?;
            let mut keys = TenantKeys::unlock(&pass, &tenant_id)
                .map_err(|e| anyhow::anyhow!("unlock: {e}"))?;
            let pubkey = keys.generate_identity();
            let sealed = keys
                .seal_identity_private()
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let id_path = identity_path(repo_root);
            if let Some(parent) = id_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&id_path, sealed)
                .with_context(|| format!("write {}", id_path.display()))?;
            // Hex of the identity public key — call this out so the user
            // can publish it and share with collaborators.
            println!("initialized encrypted repo at {}", repo_root.display());
            println!("tenant_id:     {tenant}");
            println!("identity_pub:  {}", omp_core::hex::encode(&pubkey));
            println!("identity file: {}", id_path.display());
            println!(
                "NOTE: passphrase loss is unrecoverable by design (doc 13 §Threat model)."
            );
        }
        EncCommand::Add {
            path,
            from,
            field,
            file_type,
            tenant,
            passphrase,
        } => {
            let repo = Repo::open(repo_root)?;
            let keys = unlock_with_identity(repo_root, &tenant, passphrase)?;
            let disk_path = from.unwrap_or_else(|| repo_root.join(&path));
            let bytes = std::fs::read(&disk_path)
                .with_context(|| format!("reading {}", disk_path.display()))?;
            let mut fields: Fields = BTreeMap::new();
            for (k, v) in field {
                fields.insert(k, v);
            }
            let res = repo.add_encrypted(
                &path,
                &bytes,
                Some(fields),
                file_type.as_deref(),
                &keys,
            )?;
            println!("{}", serde_json::to_string_pretty(&res)?);
        }
        EncCommand::Show {
            path,
            at,
            tenant,
            passphrase,
        } => {
            let repo = Repo::open(repo_root)?;
            let keys = unlock_with_identity(repo_root, &tenant, passphrase)?;
            let (_manifest, plaintext) = repo.show_encrypted(&path, at.as_deref(), &keys)?;
            use std::io::Write as _;
            std::io::stdout().write_all(&plaintext)?;
        }
        EncCommand::Commit {
            message,
            author,
            tenant,
            passphrase,
        } => {
            let repo = Repo::open(repo_root)?;
            let keys = unlock_with_identity(repo_root, &tenant, passphrase)?;
            let override_ = author.map(|s| {
                if let Some((name, rest)) = s.split_once('<') {
                    if let Some(email) = rest.strip_suffix('>') {
                        return AuthorOverride {
                            name: Some(name.trim().to_string()),
                            email: Some(email.trim().to_string()),
                            timestamp: None,
                        };
                    }
                }
                AuthorOverride {
                    name: Some(s),
                    email: None,
                    timestamp: None,
                }
            });
            let h = repo.commit_encrypted(&message, override_, &keys)?;
            println!("{}", h.hex());
        }
    }
    Ok(())
}

fn parse_recipient(spec: &str) -> Result<(TenantId, [u8; 32])> {
    let (tid, hex) = spec
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("expected `tenant:hex_pubkey`, got {spec:?}"))?;
    if hex.len() != 64 {
        bail!("hex_pubkey must be 64 chars (32 bytes), got {}", hex.len());
    }
    let bytes = omp_core::hex::decode(hex)
        .map_err(|e| anyhow::anyhow!("bad hex: {e}"))?;
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    let tenant = TenantId::new(tid).map_err(|e| anyhow::anyhow!("invalid tenant: {e}"))?;
    Ok((tenant, arr))
}

fn handle_share(repo_root: &Path, cmd: ShareCommand) -> Result<()> {
    match cmd {
        ShareCommand::Grant {
            path,
            to,
            tenant,
            passphrase,
        } => {
            let repo = Repo::open(repo_root)?;
            let keys = unlock_with_identity(repo_root, &tenant, passphrase)?;
            let mut recipients = Vec::new();
            for spec in to {
                recipients.push(parse_recipient(&spec)?);
            }
            let share_hash = repo.create_share(&path, None, &keys, &recipients)?;
            println!("{}", share_hash.hex());
        }
        ShareCommand::Revoke {
            path,
            keep,
            tenant,
            passphrase,
        } => {
            let repo = Repo::open(repo_root)?;
            let keys = unlock_with_identity(repo_root, &tenant, passphrase)?;
            let mut recipients = Vec::new();
            for spec in keep {
                recipients.push(parse_recipient(&spec)?);
            }
            let share_hash = repo.revoke_share(&path, &keys, &recipients)?;
            println!("{}", share_hash.hex());
        }
        ShareCommand::Pubkey {
            tenant,
            passphrase,
        } => {
            let keys = unlock_with_identity(repo_root, &tenant, passphrase)?;
            match keys.identity() {
                Some(id) => println!("{}", omp_core::hex::encode(&id.pub_key)),
                None => bail!("no identity configured — run `omp enc init` first"),
            }
        }
    }
    Ok(())
}

fn handle_admin(cmd: AdminCommand) -> Result<()> {
    match cmd {
        AdminCommand::Tenant(t) => handle_tenant(t),
    }
}

fn handle_tenant(cmd: TenantCommand) -> Result<()> {
    match cmd {
        TenantCommand::Create {
            name,
            registry,
            tenants_base,
            bytes,
            object_count,
            probe_fuel,
            wall_clock_s,
        } => {
            let path = registry_path(registry, tenants_base)?;
            let mut reg = TenantRegistry::load(&path)?;
            let quotas = Quotas {
                bytes,
                object_count,
                probe_fuel_per_request: probe_fuel,
                wall_clock_s_per_request: wall_clock_s,
                concurrent_writes: None,
            };
            let tenant = TenantId::new(&name)?;
            let token = reg.create(tenant, quotas)?;
            reg.save(&path)?;
            println!("tenant: {name}");
            println!("token:  {token}");
            println!("(token is shown once — save it now)");
        }
        TenantCommand::List {
            registry,
            tenants_base,
        } => {
            let path = registry_path(registry, tenants_base)?;
            let reg = TenantRegistry::load(&path)?;
            if reg.entries().is_empty() {
                println!("(no tenants)");
                return Ok(());
            }
            for entry in reg.entries() {
                let q = &entry.quotas;
                print!("{}  token_sha256={}", entry.id, entry.token_sha256);
                if let Some(b) = q.bytes {
                    print!(" bytes={b}");
                }
                if let Some(c) = q.object_count {
                    print!(" object_count={c}");
                }
                println!();
            }
        }
        TenantCommand::Delete {
            name,
            registry,
            tenants_base,
        } => {
            let path = registry_path(registry, tenants_base)?;
            let mut reg = TenantRegistry::load(&path)?;
            reg.remove(&TenantId::new(&name)?)?;
            reg.save(&path)?;
            println!("removed: {name}");
        }
        TenantCommand::SetQuota {
            name,
            registry,
            tenants_base,
            bytes,
            object_count,
            probe_fuel,
            wall_clock_s,
        } => {
            let path = registry_path(registry, tenants_base)?;
            let mut reg = TenantRegistry::load(&path)?;
            let quotas = Quotas {
                bytes,
                object_count,
                probe_fuel_per_request: probe_fuel,
                wall_clock_s_per_request: wall_clock_s,
                concurrent_writes: None,
            };
            reg.set_quotas(&TenantId::new(&name)?, quotas)?;
            reg.save(&path)?;
            println!("updated: {name}");
        }
    }
    Ok(())
}

fn registry_path(
    explicit: Option<PathBuf>,
    tenants_base: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    if let Some(base) = tenants_base {
        return Ok(default_registry_path(&base));
    }
    bail!("need --registry or --tenants-base")
}

fn parse_field_kv(s: &str) -> std::result::Result<(String, FieldValue), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("field must be key=value, got {s:?}"))?;
    let value = if let Ok(i) = v.parse::<i64>() {
        FieldValue::Int(i)
    } else if let Ok(f) = v.parse::<f64>() {
        FieldValue::Float(f)
    } else if v == "true" {
        FieldValue::Bool(true)
    } else if v == "false" {
        FieldValue::Bool(false)
    } else if let Some(inner) = v.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        // Simple string list: `[a,b,c]`.
        let items = inner
            .split(',')
            .map(|s| FieldValue::String(s.trim().to_string()))
            .collect();
        FieldValue::List(items)
    } else {
        FieldValue::String(v.to_string())
    };
    Ok((k.to_string(), value))
}

// Silence a warning on unused Path import in modes where it's not needed.
#[allow(dead_code)]
fn _use_path(_: &Path) {}

// --- compact JSON output ----------------------------------------------------
//
// By default we strip provenance hashes from CLI output so text-model clients
// reading the JSON aren't forced to sift through 64-char hex. `--verbose` /
// `-v` keeps the full shape (useful for replay and audit).

fn print_manifest(m: &omp_core::manifest::Manifest, verbose: bool) -> Result<()> {
    let mut v = serde_json::to_value(m)?;
    if !verbose {
        strip_manifest_keys(&mut v);
    }
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

fn print_show_result(
    r: &omp_core::api::ShowResult,
    verbose: bool,
) -> Result<()> {
    let mut v = serde_json::to_value(r)?;
    if !verbose {
        // ShowResult is an enum serialized with a `kind` tag. Strip hashes
        // inside whichever variant is present.
        if let Some(obj) = v.as_object_mut() {
            let kind = obj.get("kind").and_then(|k| k.as_str()).map(|s| s.to_string());
            match kind.as_deref() {
                Some("manifest") => {
                    if let Some(inner) = obj.get_mut("manifest") {
                        strip_manifest_keys(inner);
                    }
                }
                Some("tree") => {
                    if let Some(entries) = obj.get_mut("entries") {
                        if let Some(arr) = entries.as_array_mut() {
                            for e in arr {
                                if let Some(o) = e.as_object_mut() {
                                    o.remove("hash");
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        } else if let Some(obj) = v.as_object_mut() {
            let _ = obj; // untagged case — nothing to do
        }
    }
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

fn strip_manifest_keys(v: &mut serde_json::Value) {
    if let Some(obj) = v.as_object_mut() {
        for k in [
            "source_hash",
            "schema_hash",
            "probe_hashes",
            "ingester_version",
        ] {
            obj.remove(k);
        }
    }
}
