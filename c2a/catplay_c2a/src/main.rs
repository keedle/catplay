#![recursion_limit = "256"]

use std::process::exit;

use catplay_c2a::{Main, RuntimeOptions};
use catplay_tracing::logger::{AsyncLogger, setup_prod_logger};
use catplay_tracing::tracer::SessionTracer;
use log::{error, info, warn};

#[cfg(all(feature = "jemalloc", feature = "mimalloc"))]
compile_error!("features `jemalloc` and `mimalloc` are mutually exclusive");

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(all(feature = "jemalloc", not(target_arch = "riscv32")))]
#[global_allocator]
static GLOBAL_ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(all(feature = "jemalloc", not(target_arch = "riscv32")))]
use tikv_jemalloc_sys as _;

#[cfg(all(feature = "jemalloc", not(target_arch = "riscv32")))]
fn disable_jemalloc_thread_cache() {
    let mut enabled = false;
    let result = unsafe {
        tikv_jemalloc_sys::mallctl(
            c"thread.tcache.enabled".as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut enabled as *mut _ as *mut libc::c_void,
            std::mem::size_of_val(&enabled),
        )
    };
    if result != 0 {
        eprintln!("failed to disable jemalloc thread cache: mallctl returned {result}");
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    #[cfg(all(feature = "jemalloc", not(target_arch = "riscv32")))]
    disable_jemalloc_thread_cache();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--help"] {
        println!("catplay_c2a [--experimental-airplay-mirroring --mirror-h264-profile-idc N --mirror-h264-level-idc N]");
        println!(
            "Mirroring requires its compile feature, a connected car, and an already configured network interface. H264 values are admission gates, not measured decoder support. Audio and phone controls are unavailable."
        );
        return;
    }
    let options = match RuntimeOptions::parse(&args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            exit(2);
        }
    };
    let mirroring = options.is_mirroring();
    if mirroring {
        // Receiver debug logs and session tracing can contain peer keys/payload.
        // This experimental process intentionally installs no production logger.
        SessionTracer::disable_globally();
        println!("Experimental picture-only mirroring selected; CarPlay phone invitations disabled.");
    } else {
        setup_prod_logger();
    }
    AsyncLogger::set_async(true);
    let b = AsyncLogger::create_barrier();
    if !mirroring && std::env::var("CATPLAY_TRACING").as_deref() == Ok("1") {
        warn!("Tracing is enabled (overhead)");
        SessionTracer::enable_globally();
    }

    info!("Calling start()");

    let mut main = Main::start_with_options(options).await;
    match main {
        Err(err) => {
            error!("Failed to start: {}", err);
            if mirroring {
                eprintln!("Experimental mirroring startup failed");
            }
        }
        Ok(ref mut main) => {
            info!("Started");

            if mirroring {
                tokio::select! {
                    result = main.do_loop() => { if result.is_err() { eprintln!("Experimental mirroring stopped"); } },
                    _ = tokio::signal::ctrl_c() => {},
                }
                // Bound cleanup even if a peer stops responding during teardown.
                let _ = tokio::time::timeout(std::time::Duration::from_secs(3), main.shutdown()).await;
            } else if let Err(err) = main.do_loop().await {
                error!("Error during reconcile: {}", err);
            }
        }
    }

    drop(b);
    exit(1);
}
