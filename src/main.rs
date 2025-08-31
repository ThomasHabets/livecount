/*
* Before going live:
* * Send new messages to all clients when number changes.
*
*/
use log::info;
use std::sync::Arc;
use clap::Parser;
use warp::Filter;
//use prometheus

mod filters;
mod registry;

use registry::Registry;

#[derive(Parser, Debug)]
#[command(version)]
struct Opt {
    /// Silence all output
    #[arg(short, long)]
    quiet: bool,
    /// Verbose mode (-v, -vv, -vvv, etc)
    #[arg(short, action=clap::ArgAction::Count)]
    verbose: usize,
    /// Timestamp (sec, ms, ns, none)
    #[arg(short, long = "timestamp")]
    ts: Option<stderrlog::Timestamp>,

    #[arg(long, default_value="127.0.0.1:8000")]
    listen: std::net::SocketAddr,
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
async fn main() {
    println!("Running");

    let opt = Opt::parse();
    stderrlog::new()
        .module(module_path!())
        .quiet(opt.quiet)
        .verbosity(opt.verbose)
        .timestamp(opt.ts.unwrap_or(stderrlog::Timestamp::Second))
        .init()
        .expect("Failed to initialize logging");
    info!("Running");

    let reg = Arc::new(Registry::new());
    let api = filters::livecount(reg.clone())
        .or(warp::path!("livecount" / "metrics").and_then(metrics_handler));
    let routes = api.with(warp::log("livecount"));

    warp::serve(routes).run(opt.listen).await;
    // reg.to_owned().stop().await.expect("failed to stop()");
}
