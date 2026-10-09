//! Standalone `ninox-ptyd` host (tests and smoke runs; production uses
//! `ninox ptyd`).
//!
//! Usage:
//! - `ninox-ptyd [--socket PATH] [--checkpoint-dir DIR | --no-checkpoints] [--takeover]`
//! - `ninox-ptyd attach [--socket PATH] [--no-detach] PANE` (detach: Ctrl-Space, d)

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let mut socket = ninox_ptyd::socket_path();
    let mut checkpoints = Some(ninox_ptyd::checkpoint::default_dir());
    let mut takeover = false;
    let mut args = std::env::args().skip(1).peekable();
    if args.peek().map(String::as_str) == Some("attach") {
        args.next();
        return attach(args.collect());
    }
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => socket = PathBuf::from(args.next().ok_or_else(|| anyhow::anyhow!("--socket needs a path"))?),
            "--checkpoint-dir" => {
                checkpoints = Some(PathBuf::from(args.next().ok_or_else(|| anyhow::anyhow!("--checkpoint-dir needs a path"))?))
            }
            "--no-checkpoints" => checkpoints = None,
            "--takeover" => takeover = true,
            other => anyhow::bail!("unknown argument {other:?}"),
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("NINOX_PTYD_LOG").unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(async move {
        if takeover {
            ninox_ptyd::run_host_takeover(socket, checkpoints).await
        } else {
            ninox_ptyd::run_host(socket, checkpoints).await
        }
    });
    // Don't wait on lingering blocking tasks; pane threads are not ours to join.
    rt.shutdown_timeout(std::time::Duration::from_millis(200));
    if let Err(e) = &result {
        eprintln!("ninox-ptyd: {e:#}");
    }
    result
}

fn attach(args: Vec<String>) -> anyhow::Result<()> {
    let mut socket = ninox_ptyd::socket_path();
    let mut detach = Some((0u8, b'd'));
    let mut pane = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--socket" => socket = PathBuf::from(it.next().ok_or_else(|| anyhow::anyhow!("--socket needs a path"))?),
            "--no-detach" => detach = None,
            _ => pane = Some(a),
        }
    }
    let pane = pane.ok_or_else(|| anyhow::anyhow!("attach needs a pane id"))?;
    let rt = tokio::runtime::Runtime::new()?;
    let opts = ninox_ptyd::attach::AttachOptions { detach, force_no_tty: false };
    let end = rt.block_on(ninox_ptyd::attach::attach(&socket, &pane, opts))?;
    rt.shutdown_timeout(std::time::Duration::from_millis(100));
    eprintln!("attach ended: {end:?}");
    match end {
        ninox_ptyd::attach::AttachEnd::Detached => Ok(()),
        ninox_ptyd::attach::AttachEnd::PaneExited(code) => std::process::exit(code.unwrap_or(0).clamp(0, 255) + 100),
        ninox_ptyd::attach::AttachEnd::HostGone => std::process::exit(99),
    }
}
