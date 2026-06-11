// Allow single element loop to make metric initialization consistent.
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::Parser;
use log::info;
use warp::Filter;
//use prometheus

mod filters;
mod handoff;
mod registry;

use registry::Registry;

#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[derive(Parser, Debug)]
#[command(version)]
struct Opt {
    /// Silence all output
    #[arg(short, long)]
    quiet: bool,

    /// Verbose mode.
    #[arg(short, default_value = "info")]
    verbose: LogLevel,

    /// Timestamp (sec, ms, ns, none)
    #[arg(short, long = "timestamp")]
    ts: Option<stderrlog::Timestamp>,

    /// Listen directly on TCP/TLS at this address.
    #[arg(long)]
    listen: Option<std::net::SocketAddr>,

    /// Listen for plain HTTP socket handoffs on this Unix socket.
    #[arg(long)]
    unix_listen: Option<PathBuf>,

    /// Change the Unix socket group after binding.
    #[arg(long, requires = "unix_listen")]
    unix_listen_group: Option<String>,

    /// Change the Unix socket mode after binding, parsed as octal.
    #[arg(long, value_parser = parse_octal_mode, requires = "unix_listen")]
    unix_listen_mode: Option<u32>,

    /// TLS certificate for the direct TCP listener.
    #[arg(long)]
    cert: Option<PathBuf>,

    /// TLS private key for the direct TCP listener.
    #[arg(long)]
    key: Option<PathBuf>,
}

fn parse_octal_mode(value: &str) -> std::result::Result<u32, String> {
    let mode =
        u32::from_str_radix(value, 8).map_err(|_| format!("invalid octal mode {value:?}"))?;
    if mode > 0o7777 {
        return Err(format!("mode {value:?} is larger than 7777"));
    }
    Ok(mode)
}

async fn metrics_handler() -> Result<impl warp::Reply, warp::Rejection> {
    use prometheus::Encoder;
    let encoder = prometheus::TextEncoder::new();

    let mut buffer = Vec::new();
    if let Err(e) = encoder.encode(&registry::REGISTRY.gather(), &mut buffer) {
        eprintln!("could not encode custom metrics: {}", e);
    };
    let mut res = match String::from_utf8(buffer.clone()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("custom metrics could not be from_utf8'd: {}", e);
            String::default()
        }
    };
    buffer.clear();

    let mut buffer = Vec::new();
    if let Err(e) = encoder.encode(&prometheus::gather(), &mut buffer) {
        eprintln!("could not encode prometheus metrics: {}", e);
    };
    let res_custom = match String::from_utf8(buffer.clone()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("prometheus metrics could not be from_utf8'd: {}", e);
            String::default()
        }
    };
    buffer.clear();

    res.push_str(&res_custom);
    Ok(res)
}

#[tokio::main]
async fn main() -> Result<()> {
    println!(
        "Livecount {} built with {} ({})",
        env!("GIT_VERSION"),
        env!("RUSTC_VERSION"),
        env!("BUILD_PROFILE")
    );

    let opt = Opt::parse();
    stderrlog::new()
        .module(module_path!())
        .quiet(opt.quiet)
        .verbosity(opt.verbose as usize)
        .timestamp(opt.ts.unwrap_or(stderrlog::Timestamp::Second))
        .init()
        .expect("Failed to initialize logging");
    info!("Running");

    let reg = Arc::new(Registry::new());
    let api = filters::livecount(reg.clone())
        .or(warp::path!("livecount" / "metrics").and_then(metrics_handler));
    let routes = api.with(warp::log("livecount"));

    let tcp = if let Some(listen) = opt.listen {
        let cert = opt
            .cert
            .clone()
            .context("--cert is required when --listen is used")?;
        let key = opt
            .key
            .clone()
            .context("--key is required when --listen is used")?;
        Some((listen, cert, key))
    } else {
        None
    };

    if opt.unix_listen.is_none() && tcp.is_none() {
        bail!("at least one of --unix-listen or --listen must be specified");
    }

    let unix = opt
        .unix_listen
        .as_ref()
        .map(|path| {
            let listener = handoff::bind(path)
                .with_context(|| format!("failed to bind Unix socket {}", path.display()))?;
            handoff::configure_socket_path(
                path,
                opt.unix_listen_group.as_deref(),
                opt.unix_listen_mode,
            )
            .with_context(|| {
                format!(
                    "failed to configure Unix socket group or mode for {}",
                    path.display()
                )
            })?;
            Ok::<_, anyhow::Error>((path.clone(), listener))
        })
        .transpose()?;

    match (unix, tcp) {
        (Some((path, listener)), Some((listen, cert, key))) => {
            info!("Listening for socket handoffs on {}", path.display());
            info!("Listening directly on TCP/TLS at {listen}");
            let unix_server = warp::serve(routes.clone()).run_incoming(handoff::incoming(listener));
            let tcp_server = warp::serve(routes)
                .tls()
                .cert_path(cert)
                .key_path(key)
                .run(listen);
            tokio::join!(unix_server, tcp_server);
        }
        (Some((path, listener)), None) => {
            info!("Listening for socket handoffs on {}", path.display());
            warp::serve(routes)
                .run_incoming(handoff::incoming(listener))
                .await;
        }
        (None, Some((listen, cert, key))) => {
            info!("Listening directly on TCP/TLS at {listen}");
            warp::serve(routes)
                .tls()
                .cert_path(cert)
                .key_path(key)
                .run(listen)
                .await;
        }
        (None, None) => unreachable!("listener configuration was already validated"),
    }

    // Plain HTTP serving.
    // warp::serve(routes).run(opt.listen).await;
    // reg.to_owned().stop().await.expect("failed to stop()");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_octal_mode;

    #[test]
    fn parses_unix_socket_mode_as_octal() {
        assert_eq!(parse_octal_mode("660").unwrap(), 0o660);
        assert_eq!(parse_octal_mode("0660").unwrap(), 0o660);
        assert_eq!(parse_octal_mode("1777").unwrap(), 0o1777);
    }

    #[test]
    fn rejects_invalid_unix_socket_modes() {
        assert!(parse_octal_mode("668").is_err());
        assert!(parse_octal_mode("10000").is_err());
    }
}
