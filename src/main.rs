//! `syncbook` pulls a reMarkable tablet notebook's pages down over SSH and
//! renders them to PNG/SVG, and pushes a single edited `.rm` page file back
//! up safely (backup + hash-verify). Both directions connect through the
//! same `~/.ssh/config` host alias that plain `ssh`/`scp` on the command
//! line would use, so nothing reMarkable-specific needs to be configured
//! beyond that one alias -- see [`ensure_ssh_host_configured`] for the
//! first-run setup wizard that writes it.

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Absolute path on the reMarkable device to xochitl's notebook data store.
/// Every notebook's `<uuid>.content`/`<uuid>.metadata` files and its
/// per-page `<uuid>/<page-uuid>.rm` files live directly under here.
const REMOTE_XOCHITL: &str = "/home/root/.local/share/remarkable/xochitl";

/// Top-level CLI arguments, parsed by `clap`'s derive macro.
#[derive(Parser)]
#[command(name = "syncbook", version, about = "Pull and push reMarkable notebooks over SSH")]
struct Cli {
    /// SSH host alias to use (must resolve via ~/.ssh/config)
    #[arg(long, global = true, default_value = "remarkable")]
    host: String,

    #[command(subcommand)]
    command: Commands,
}

/// The two subcommands syncbook supports: pulling pages down for viewing,
/// and pushing one edited page back up.
#[derive(Subcommand)]
enum Commands {
    /// Pull a notebook's pages down and render them to PNG/SVG.
    Pullrm {
        /// Notebook visibleName (as shown in the reMarkable UI) or its UUID.
        notebook: String,
        /// Output directory (default: `./<notebook name>`)
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Push a local .rm page file up to a notebook, safely (backup + verify).
    Pushrm {
        /// Notebook visibleName or UUID.
        notebook: String,
        /// UUID of the page within the notebook to replace.
        page: String,
        /// Local .rm file to push.
        file: PathBuf,
    },
}

/// Entry point: parses CLI args, makes sure the configured SSH host alias
/// is set up (running the first-run wizard if not), then dispatches to
/// [`pullrm`] or [`pushrm`].
///
/// # Errors
/// Returns an error if host setup fails, or if the dispatched subcommand
/// does.
fn main() -> Result<()> {
    let cli = Cli::parse();
    ensure_ssh_host_configured(&cli.host)?;

    match cli.command {
        Commands::Pullrm { notebook, output } => pullrm(&cli.host, &notebook, output),
        Commands::Pushrm { notebook, page, file } => pushrm(&cli.host, &notebook, &page, &file),
    }
}

// --- first-run config wizard -------------------------------------------------

/// Both pullrm and pushrm connect via the same `~/.ssh/config` host alias
/// (default "remarkable") rather than a syncbook-specific config file, so
/// plain `ssh`/`scp` on the command line work identically to what syncbook
/// does internally. If the alias isn't configured yet, prompt once and
/// append a managed block.
fn ensure_ssh_host_configured(host: &str) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot determine home directory"))?;
    let ssh_dir = home.join(".ssh");
    let config_path = ssh_dir.join("config");

    let existing = fs::read_to_string(&config_path).unwrap_or_default();
    if host_block_exists(&existing, host) {
        return Ok(());
    }

    eprintln!(
        "No '{host}' entry found in ~/.ssh/config -- first-time setup for syncbook.\n\
         See README.md for how to enable SSH on the reMarkable and install your key first."
    );

    let hostname = prompt(&format!("reMarkable hostname or IP: "))?;
    if hostname.trim().is_empty() {
        bail!("hostname/IP is required");
    }
    let default_key = home.join(".ssh/id_ed25519");
    let key_prompt = format!(
        "Path to SSH private key [{}]: ",
        default_key.display()
    );
    let key_input = prompt(&key_prompt)?;
    let key_path = if key_input.trim().is_empty() {
        default_key
    } else {
        PathBuf::from(shellexpand_tilde(&key_input, &home))
    };

    fs::create_dir_all(&ssh_dir).context("creating ~/.ssh")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o700)).ok();
    }

    let block = format!(
        "\n# BEGIN SYNCBOOK MANAGED ({host})\n\
         Host {host}\n    \
         HostName {hostname}\n    \
         User root\n    \
         IdentityFile {key}\n    \
         StrictHostKeyChecking accept-new\n\
         # END SYNCBOOK MANAGED ({host})\n",
        host = host,
        hostname = hostname.trim(),
        key = key_path.display(),
    );

    let is_new = !config_path.exists();
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config_path)
        .context("opening ~/.ssh/config")?;
    f.write_all(block.as_bytes())?;

    if is_new {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).ok();
        }
    }

    eprintln!("Wrote Host '{host}' to ~/.ssh/config. Re-run your command to continue.");
    std::process::exit(0);
}

/// Returns true if `config_text` already has a `Host` line listing `host`
/// among its space-separated patterns (SSH config allows more than one
/// pattern per `Host` line, e.g. `Host foo bar`).
fn host_block_exists(config_text: &str, host: &str) -> bool {
    config_text.lines().any(|line| {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Host ") {
            rest.split_whitespace().any(|pattern| pattern == host)
        } else {
            false
        }
    })
}

/// Expands a leading `~/` in `input` to `home`; returns `input` unchanged
/// otherwise. A minimal stand-in for shell tilde expansion, since this
/// value comes from an interactive prompt rather than an actual shell.
fn shellexpand_tilde(input: &str, home: &Path) -> String {
    let input = input.trim();
    if let Some(rest) = input.strip_prefix("~/") {
        home.join(rest).to_string_lossy().into_owned()
    } else {
        input.to_string()
    }
}

/// Writes `message` to stderr (keeping stdout clean for piping) and reads
/// back one line of input with the trailing newline stripped.
///
/// # Errors
/// Returns an error if reading from stdin fails.
fn prompt(message: &str) -> Result<String> {
    eprint!("{message}");
    io::stderr().flush().ok();
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

// --- ssh/scp helpers ---------------------------------------------------------

/// Runs `remote_command` on `host` via `ssh` and returns its captured
/// stdout.
///
/// # Errors
/// Returns an error if the `ssh` process can't be spawned, or exits
/// non-zero (its stderr is included in the returned error).
fn ssh_output(host: &str, remote_command: &str) -> Result<String> {
    let out = Command::new("ssh")
        .arg("-o")
        .arg("ConnectTimeout=8")
        .arg(host)
        .arg(remote_command)
        .output()
        .context("running ssh")?;
    if !out.status.success() {
        bail!(
            "ssh command failed ({}): {}",
            remote_command,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Runs `remote_command` on `host` via `ssh`, letting its stdout/stderr
/// pass through to the terminal instead of capturing them.
///
/// # Errors
/// Returns an error if the `ssh` process can't be spawned, or exits
/// non-zero.
fn ssh_run(host: &str, remote_command: &str) -> Result<()> {
    let status = Command::new("ssh")
        .arg("-o")
        .arg("ConnectTimeout=8")
        .arg(host)
        .arg(remote_command)
        .status()
        .context("running ssh")?;
    if !status.success() {
        bail!("ssh command failed: {}", remote_command);
    }
    Ok(())
}

/// Copies `remote_path` on `host` down to `local_path` via `scp`.
///
/// # Errors
/// Returns an error if `scp` can't be spawned or exits non-zero.
fn scp_down(host: &str, remote_path: &str, local_path: &Path) -> Result<()> {
    let status = Command::new("scp")
        .arg("-o")
        .arg("ConnectTimeout=8")
        .arg(format!("{host}:{remote_path}"))
        .arg(local_path)
        .status()
        .context("running scp (download)")?;
    if !status.success() {
        bail!("scp download failed: {remote_path}");
    }
    Ok(())
}

/// Copies `local_path` up to `remote_path` on `host` via `scp`.
///
/// # Errors
/// Returns an error if `scp` can't be spawned or exits non-zero.
fn scp_up(host: &str, local_path: &Path, remote_path: &str) -> Result<()> {
    let status = Command::new("scp")
        .arg("-o")
        .arg("ConnectTimeout=8")
        .arg(local_path)
        .arg(format!("{host}:{remote_path}"))
        .status()
        .context("running scp (upload)")?;
    if !status.success() {
        bail!("scp upload failed: {remote_path}");
    }
    Ok(())
}

/// Single-quote a value for safe interpolation into a remote shell command
/// string (the remote end is BusyBox ash via `ssh host '<command>'`).
fn shq(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

// --- notebook lookup ----------------------------------------------------------

/// Returns true if `s` has the canonical UUID shape (five hyphen-separated
/// hex groups, lengths 8-4-4-4-12) -- the format reMarkable uses for both
/// notebook and page identifiers.
fn looks_like_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(parts.iter())
            .all(|(len, part)| part.len() == *len && part.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Resolves `notebook` to a UUID. Returned as-is if it already
/// [`looks_like_uuid`]; otherwise grep'd for on the device by matching its
/// `visibleName` field across every `*.metadata` file under
/// [`REMOTE_XOCHITL`].
///
/// # Errors
/// Returns an error if no notebook matches, or if more than one does --
/// in the latter case the caller should pass the UUID directly instead.
fn find_notebook_uuid(host: &str, notebook: &str) -> Result<String> {
    if looks_like_uuid(notebook) {
        return Ok(notebook.to_string());
    }
    let needle = format!("\"visibleName\": \"{}\"", notebook.replace('"', "\\\""));
    let cmd = format!(
        "grep -l {} {}/*.metadata 2>/dev/null",
        shq(&needle),
        REMOTE_XOCHITL
    );
    let out = ssh_output(host, &cmd)?;
    let matches: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    match matches.len() {
        0 => bail!("no notebook named '{notebook}' found on the device"),
        1 => {
            let path = matches[0];
            let uuid = Path::new(path)
                .file_stem()
                .ok_or_else(|| anyhow!("unexpected metadata path: {path}"))?
                .to_string_lossy()
                .into_owned();
            Ok(uuid)
        }
        _ => bail!(
            "multiple notebooks named '{notebook}' found ({} matches) -- use the UUID instead",
            matches.len()
        ),
    }
}

/// Reads and parses a notebook's `<uuid>.content` file from the device --
/// this is where the ordered list of page ids (`cPages.pages`) lives.
///
/// # Errors
/// Returns an error if the file can't be read over SSH or isn't valid JSON.
fn fetch_content(host: &str, uuid: &str) -> Result<serde_json::Value> {
    let raw = ssh_output(host, &format!("cat {REMOTE_XOCHITL}/{uuid}.content"))?;
    serde_json::from_str(&raw).context("parsing .content JSON")
}

// --- rendering ----------------------------------------------------------------

/// Renders via `rmc` (SVG) then `cairosvg` (PNG), both run through `uv run`
/// so no manual pip/venv setup is needed. This shells out to the same
/// Python pipeline validated by hand, rather than reimplementing reMarkable's
/// reverse-engineered lines-v6 format natively in Rust.
fn render_rm(rm_path: &Path, svg_path: &Path, png_path: &Path) -> Result<()> {
    let status = Command::new("uv")
        .args(["run", "--with", "rmc", "rmc"])
        .arg(rm_path)
        .args(["-t", "svg", "-o"])
        .arg(svg_path)
        .status()
        .context("running `uv run rmc` -- is uv installed? see README prerequisites")?;
    if !status.success() {
        bail!("rmc conversion failed for {}", rm_path.display());
    }

    let py = format!(
        "import cairosvg; cairosvg.svg2png(url={:?}, write_to={:?}, output_width=1404, output_height=1872, background_color='white')",
        svg_path, png_path
    );
    let status = Command::new("uv")
        .args(["run", "--with", "cairosvg", "python3", "-c"])
        .arg(&py)
        .status()
        .context("running `uv run cairosvg`")?;
    if !status.success() {
        bail!("PNG render failed for {}", svg_path.display());
    }
    Ok(())
}

// --- pullrm ---------------------------------------------------------------

/// Pulls every page of `notebook` down from `host`, renders each to
/// PNG/SVG via [`render_rm`], and writes a copy of the notebook's
/// `.content` JSON alongside them in `output` (default: a sanitized
/// version of `notebook`'s name in the current directory).
///
/// A page referenced by `.content` whose `.rm` file is missing on the
/// device is reported and skipped rather than aborting the whole pull --
/// see the comment at the skip site for how that situation arises.
///
/// # Errors
/// Returns an error if the notebook can't be resolved, its `.content`
/// can't be fetched or parsed, or any page's download/render fails.
fn pullrm(host: &str, notebook: &str, output: Option<PathBuf>) -> Result<()> {
    let uuid = find_notebook_uuid(host, notebook)?;
    let content = fetch_content(host, &uuid)?;
    let pages = content["cPages"]["pages"]
        .as_array()
        .ok_or_else(|| anyhow!("unexpected .content shape: no cPages.pages array"))?;

    let out_dir = output.unwrap_or_else(|| PathBuf::from(sanitize_filename(notebook)));
    fs::create_dir_all(&out_dir)?;

    println!("Notebook '{notebook}' ({uuid}) -- {} page(s)", pages.len());

    for (i, page) in pages.iter().enumerate() {
        let page_id = page["id"]
            .as_str()
            .ok_or_else(|| anyhow!("page entry missing id"))?;
        let remote_rm = format!("{REMOTE_XOCHITL}/{uuid}/{page_id}.rm");

        // .content's page list can reference a page whose .rm file no
        // longer exists on disk -- observed in practice after deleting a
        // stray page and then reopening the notebook, which silently
        // re-added the reference (xochitl logs "loaded pageId=... - no
        // file found" but doesn't repair .content). Skip rather than abort
        // the whole pull over one such entry.
        if ssh_output(host, &format!("test -e {} && echo yes", shq(&remote_rm)))
            .unwrap_or_default()
            .trim()
            != "yes"
        {
            println!("  page {}: {page_id} -- SKIPPED (no .rm file on device)", i + 1);
            continue;
        }

        let rm_path = out_dir.join(format!("{page_id}.rm"));
        let svg_path = out_dir.join(format!("{page_id}.svg"));
        let png_path = out_dir.join(format!("{page_id}.png"));

        scp_down(host, &remote_rm, &rm_path)?;
        render_rm(&rm_path, &svg_path, &png_path)?;

        println!("  page {}: {}", i + 1, png_path.display());
    }

    let content_path = out_dir.join("content.json");
    fs::write(&content_path, serde_json::to_string_pretty(&content)?)?;

    Ok(())
}

/// Replaces every character that isn't alphanumeric, `-`, or `_` with `_`,
/// so a notebook's `visibleName` can double as a filesystem-safe default
/// output directory name.
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

// --- pushrm -----------------------------------------------------------------

/// Replaces one page's `.rm` file in `notebook` on `host` with `file`,
/// safely: backs up the notebook's current `.metadata`, `.content`, and
/// (if it already exists) the target page's `.rm` file first; uploads to
/// a `.new` temp path and renames it into place so a failed transfer
/// can't leave a half-written page file; then reads the pushed file back
/// and compares its SHA-256 against the local original.
///
/// # Errors
/// Returns an error if the notebook can't be resolved, any backup or
/// upload step fails, or the post-push hash verification doesn't match --
/// in which case the backup taken at the start of the call is left in
/// place for manual recovery.
fn pushrm(host: &str, notebook: &str, page: &str, file: &Path) -> Result<()> {
    let uuid = find_notebook_uuid(host, notebook)?;
    let remote_page_path = format!("{REMOTE_XOCHITL}/{uuid}/{page}.rm");

    let ts = current_timestamp();
    let backup_dir = dirs::home_dir()
        .ok_or_else(|| anyhow!("no home dir"))?
        .join("remarkable-backups")
        .join(&ts)
        .join(&uuid);
    fs::create_dir_all(&backup_dir)?;

    println!("Backing up current state to {}", backup_dir.display());
    scp_down(
        host,
        &format!("{REMOTE_XOCHITL}/{uuid}.metadata"),
        &backup_dir.join(format!("{uuid}.metadata")),
    )?;
    scp_down(
        host,
        &format!("{REMOTE_XOCHITL}/{uuid}.content"),
        &backup_dir.join(format!("{uuid}.content")),
    )?;
    if ssh_output(host, &format!("test -e {remote_page_path} && echo yes")).unwrap_or_default().trim() == "yes" {
        scp_down(host, &remote_page_path, &backup_dir.join(format!("{page}.rm")))?;
    } else {
        println!("  (page {page} does not exist yet on the device -- this will create it)");
    }

    println!("Pushing {} -> {}", file.display(), remote_page_path);
    let remote_tmp = format!("{remote_page_path}.new");
    scp_up(host, file, &remote_tmp)?;
    ssh_run(host, &format!("mv {} {}", shq(&remote_tmp), shq(&remote_page_path)))?;

    println!("Verifying...");
    let verify_path = backup_dir.join(format!("{page}.rm.pushed-verify"));
    scp_down(host, &remote_page_path, &verify_path)?;
    let local_hash = sha256_file(file)?;
    let remote_hash = sha256_file(&verify_path)?;
    if local_hash != remote_hash {
        bail!(
            "VERIFY FAILED: hash mismatch after push. Backup is in {}. \
             The device file may be corrupt -- restore from backup before opening the notebook.",
            backup_dir.display()
        );
    }
    fs::remove_file(&verify_path).ok();

    println!(
        "Done. Hash-verified on device. Open (or close and reopen) the notebook on the \
         tablet to see the change -- xochitl only re-parses a page's file when its \
         document is opened, so nothing further needs to be sent."
    );
    Ok(())
}

/// Returns the current UTC time as `YYYYMMDDTHHMMSSZ`, used to namespace
/// each push's backup directory.
///
/// # Panics
/// Panics if the `date` command can't be spawned (see the `.expect()`
/// below) -- this is treated as an environment precondition, not a
/// recoverable error.
fn current_timestamp() -> String {
    let out = Command::new("date")
        .args(["-u", "+%Y%m%dT%H%M%SZ"])
        .output()
        .expect("date command should always be available");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Reads `path` fully into memory and returns its SHA-256 digest as a
/// lowercase hex string.
///
/// # Errors
/// Returns an error if the file can't be read.
fn sha256_file(path: &Path) -> Result<String> {
    let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Ok(format!("{:x}", hasher.finalize()))
}
