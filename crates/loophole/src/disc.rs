use loophole_vm::driver_disc::{self, Disc};

pub enum Update {
    Current,
    Downloading(String, u8),
    Done(Disc),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Pinned,
    Checking,
    Current,
    Downloading(String, u8),
    Failed(String),
}

// The driver drive's disc: the one LOOPHOLE_DRIVER_DISC names, or the newest build downloaded.
pub struct DriverDisc {
    pub build: Option<String>,
    pub status: Status,
}

impl DriverDisc {
    pub fn new() -> Self {
        if driver_disc::pinned() {
            Self {
                build: None,
                status: Status::Pinned,
            }
        } else {
            Self {
                build: driver_disc::cached().map(|d| d.build),
                status: Status::Checking,
            }
        }
    }

    pub fn name(&self) -> String {
        self.build
            .clone()
            .unwrap_or_else(|| "loophole-guest.iso".to_owned())
    }

    pub fn hint(&self, available: bool) -> String {
        match (&self.status, &self.build) {
            (Status::Pinned, _) if available => "Holds loophole-setup.exe".into(),
            (Status::Pinned, _) => "LOOPHOLE_DRIVER_DISC names no file".into(),
            (Status::Downloading(next, percent), Some(build)) => {
                format!("{build} · downloading {next}, {percent}%")
            },
            (Status::Downloading(next, percent), None) => format!("Downloading {next}, {percent}%"),
            (_, Some(build)) => format!("{build} · holds loophole-setup.exe"),
            (Status::Checking | Status::Current, None) => "Looking for the latest build…".into(),
            (Status::Failed(why), None) => format!("Not downloaded: {why}"),
        }
    }
}

// On a worker thread: the latest release, downloaded unless it is `have`.
pub fn fetch(have: Option<&str>, send: impl Fn(Update)) {
    let release = match driver_disc::latest() {
        Ok(r) => r,
        Err(e) => return send(Update::Failed(format!("{e:#}"))),
    };
    if have == Some(release.build.as_str()) {
        return send(Update::Current);
    }
    let mut shown = 0;
    send(Update::Downloading(release.build.clone(), shown));
    let done = driver_disc::download(&release, |received, size| {
        let percent = (received * 100 / size.max(1)).min(100) as u8;
        if percent != shown {
            shown = percent;
            send(Update::Downloading(release.build.clone(), percent));
        }
    });
    send(match done {
        Ok(disc) => Update::Done(disc),
        Err(e) => Update::Failed(format!("{e:#}")),
    });
}
