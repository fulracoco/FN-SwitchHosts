#[cfg(not(unix))]
use anyhow::bail;
use anyhow::{ensure, Context, Result};
use std::{collections::HashMap, path::PathBuf};
use switchhosts_fnos::{
    api::{self, Auth},
    service::Service,
    target::Target,
};

fn options() -> Result<HashMap<String, String>> {
    let mut options = HashMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(key) = args.next() {
        if key == "--help" {
            println!("switchhosts-fnos --development --data-dir DIR --hosts-file FILE [--listen 127.0.0.1:50761]\nSet SWH_DEV_TOKEN to at least 32 characters.\nLinux fnOS: --data-dir DIR --gateway-socket PATH --writer-socket PATH --uid UID --gid GID\nPrivileged writer: --writer --socket PATH --uid UID --gid GID");
            std::process::exit(0);
        }
        ensure!(
            matches!(
                key.as_str(),
                "--development"
                    | "--writer"
                    | "--data-dir"
                    | "--hosts-file"
                    | "--listen"
                    | "--gateway-socket"
                    | "--writer-socket"
                    | "--socket"
                    | "--uid"
                    | "--gid"
            ),
            "unknown option: {key}"
        );
        let value = if key == "--development" || key == "--writer" {
            String::new()
        } else {
            args.next().context("option requires a value")?
        };
        ensure!(options.insert(key, value).is_none(), "duplicate option");
    }
    Ok(options)
}

fn required(options: &HashMap<String, String>, key: &str) -> Result<String> {
    options
        .get(key)
        .cloned()
        .with_context(|| format!("missing {key}"))
}

fn main() -> Result<()> {
    let options = options()?;
    #[cfg(unix)]
    if options.contains_key("--writer") {
        ensure!(
            !options.contains_key("--development") && !options.contains_key("--hosts-file"),
            "writer always targets /etc/hosts"
        );
        let uid: u32 = required(&options, "--uid")?.parse()?;
        let gid: u32 = required(&options, "--gid")?.parse()?;
        ensure!(
            uid != 0 && gid != 0,
            "writer peer must be a non-root package user"
        );
        return tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(switchhosts_fnos::target::run_writer(
                &PathBuf::from(required(&options, "--socket")?),
                uid,
                gid,
            ));
    }
    if options.contains_key("--development") {
        ensure!(
            !options.contains_key("--writer")
                && !options.contains_key("--writer-socket")
                && !options.contains_key("--gateway-socket"),
            "development cannot use production sockets"
        );
        let address: std::net::SocketAddr = options
            .get("--listen")
            .map(String::as_str)
            .unwrap_or("127.0.0.1:50761")
            .parse()?;
        ensure!(
            address.ip().is_loopback(),
            "development server must bind a loopback address"
        );
        let token = std::env::var("SWH_DEV_TOKEN")
            .context("set SWH_DEV_TOKEN before starting development")?;
        ensure!(
            token.len() >= 32,
            "SWH_DEV_TOKEN must contain at least 32 characters"
        );
        let target = PathBuf::from(required(&options, "--hosts-file")?);
        let target =
            std::fs::canonicalize(target).context("development hosts file must already exist")?;
        ensure!(
            target != std::fs::canonicalize("/etc/hosts").unwrap_or_default(),
            "development mode refuses the system hosts file"
        );
        #[cfg(windows)]
        if let Ok(windows) = std::env::var("SystemRoot") {
            let system = PathBuf::from(windows).join("System32/drivers/etc/hosts");
            ensure!(
                target.to_string_lossy().to_lowercase() != system.to_string_lossy().to_lowercase(),
                "development mode refuses the system hosts file"
            );
        }
        return tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(async {
                let service = Service::open(
                    PathBuf::from(required(&options, "--data-dir")?),
                    Target::DevelopmentFile(target),
                )
                .await?;
                start_scanner(service.clone());
                let listener = tokio::net::TcpListener::bind(address).await?;
                println!("Open http://{}/app/switchhosts/", listener.local_addr()?);
                axum::serve(
                    listener,
                    api::router(service, Auth::DevelopmentToken(token)),
                )
                .with_graceful_shutdown(shutdown())
                .await?;
                Ok(())
            });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(unsafe { libc::geteuid() } == 0, "production bootstrap must bind its root-only gateway socket before dropping privileges");
        ensure!(
            !options.contains_key("--listen") && !options.contains_key("--hosts-file"),
            "production uses gateway and writer sockets exclusively"
        );
        let uid: u32 = required(&options, "--uid")?.parse()?;
        let gid: u32 = required(&options, "--gid")?.parse()?;
        ensure!(
            uid != 0 && gid != 0,
            "web service must run as a non-root package user"
        );
        let path = PathBuf::from(required(&options, "--gateway-socket")?);
        switchhosts_fnos::target::secure_socket_parent(&path)?;
        let listener = std::os::unix::net::UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        // Execute before creating a multithreaded runtime. The root-owned
        // gateway socket accepts only the trusted root system proxy.
        ensure!(
            unsafe { libc::setgroups(0, std::ptr::null()) } == 0,
            "clear supplementary groups failed"
        );
        ensure!(
            unsafe { libc::setgid(gid) } == 0 && unsafe { libc::setuid(uid) } == 0,
            "drop package privileges failed"
        );
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(async {
                let service = Service::open(
                    PathBuf::from(required(&options, "--data-dir")?),
                    Target::WriterSocket(PathBuf::from(required(&options, "--writer-socket")?)),
                )
                .await?;
                start_scanner(service.clone());
                axum::serve(
                    tokio::net::UnixListener::from_std(listener)?,
                    api::router(service, Auth::Gateway),
                )
                .with_graceful_shutdown(shutdown())
                .await?;
                Ok(())
            })
    }
    #[cfg(not(unix))]
    bail!("production requires Linux; use --development with an explicit test file")
}

fn start_scanner(service: std::sync::Arc<Service>) {
    tokio::spawn(async move {
        loop {
            if !service.recovery_required() {
                service.scan_once().await;
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
