use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(fallback),
    }
}

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("loophole/vms")
}

pub fn data_dir(name: &str) -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share")
        .join("loophole/vms")
        .join(name)
}

pub fn driver_disc_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join("loophole/guest")
}

pub fn runtime_dir(name: &str) -> PathBuf {
    let base = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => std::env::temp_dir().join(format!("loophole-{}", rustix::process::getuid().as_raw())),
    };
    base.join("loophole").join(name)
}
