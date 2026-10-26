use std::collections::VecDeque;
use std::fmt;
use std::io::{BufRead as _, BufReader, ErrorKind, IoSlice, Write as _};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg};
use serde::de::{DeserializeOwned, IgnoredAny};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(
    tag = "execute",
    content = "arguments",
    rename_all_fields = "kebab-case"
)]
pub enum Command {
    #[serde(rename = "qmp_capabilities")]
    QmpCapabilities,
    #[serde(rename = "query-status")]
    QueryStatus,
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "cont")]
    Cont,
    #[serde(rename = "system_powerdown")]
    SystemPowerdown,
    #[serde(rename = "system_reset")]
    SystemReset,
    #[serde(rename = "quit")]
    Quit,
    #[serde(rename = "send-key")]
    SendKey { keys: Vec<KeyValue> },
    #[serde(rename = "blockdev-open-tray")]
    BlockdevOpenTray { id: &'static str, force: bool },
    #[serde(rename = "blockdev-remove-medium")]
    BlockdevRemoveMedium { id: &'static str },
    #[serde(rename = "blockdev-close-tray")]
    BlockdevCloseTray { id: &'static str },
    #[serde(rename = "blockdev-change-medium")]
    BlockdevChangeMedium {
        id: &'static str,
        filename: PathBuf,
        format: &'static str,
        read_only_mode: ReadOnlyMode,
        force: bool,
    },
    #[serde(rename = "getfd")]
    Getfd { fdname: &'static str },
    #[serde(rename = "add_client")]
    AddClient {
        protocol: &'static str,
        fdname: &'static str,
    },
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReadOnlyMode {
    Retain,
    ReadOnly,
    ReadWrite,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type", content = "data", rename_all = "lowercase")]
pub enum KeyValue {
    Qcode(&'static str),
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct StatusInfo {
    pub running: bool,
    pub status: String,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "event")]
pub enum Event {
    #[serde(rename = "STOP")]
    Stop,
    #[serde(rename = "RESUME")]
    Resume,
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct QmpError {
    pub class: String,
    pub desc: String,
}

impl fmt::Display for QmpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.desc)
    }
}

impl std::error::Error for QmpError {}

#[derive(Deserialize)]
struct Message<R> {
    #[serde(rename = "QMP")]
    greeting: Option<IgnoredAny>,
    #[serde(rename = "return")]
    ret: Option<R>,
    error: Option<QmpError>,
    event: Option<IgnoredAny>,
}

#[derive(Deserialize)]
struct Execute<'a> {
    execute: &'a str,
}

pub struct Qmp {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    pending: VecDeque<Event>,
    // What a read timeout cut short. The rest of the line follows.
    partial: String,
}

impl Qmp {
    pub fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .with_context(|| format!("connecting to {}", path.display()))?;
        Self::from_stream(stream).with_context(|| path.display().to_string())
    }

    fn from_stream(stream: UnixStream) -> Result<Self> {
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let reader = BufReader::new(stream.try_clone()?);
        let mut qmp = Self {
            stream,
            reader,
            pending: VecDeque::new(),
            partial: String::new(),
        };
        let greeting: Message<IgnoredAny> = qmp.read()?.context("QMP connection closed")?;
        if greeting.greeting.is_none() {
            bail!("not a QMP socket");
        }
        qmp.run(&Command::QmpCapabilities)?;
        Ok(qmp)
    }

    fn read_line(&mut self) -> Result<Option<String>> {
        if self.reader.read_line(&mut self.partial)? == 0 {
            return Ok(None);
        }
        Ok(Some(std::mem::take(&mut self.partial)))
    }

    fn read<T>(&mut self) -> Result<Option<T>>
    where
        T: DeserializeOwned,
    {
        self.read_line()?
            .map(|line| serde_json::from_str(&line).map_err(Into::into))
            .transpose()
    }

    fn send(&mut self, text: &str, fd: Option<&UnixStream>) -> Result<()> {
        let Some(fd) = fd else {
            self.stream.write_all(text.as_bytes())?;
            return Ok(());
        };
        let fds = [fd.as_fd()];
        let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        control.push(SendAncillaryMessage::ScmRights(&fds));
        sendmsg(
            &self.stream,
            &[IoSlice::new(text.as_bytes())],
            &mut control,
            SendFlags::empty(),
        )?;
        Ok(())
    }

    fn execute_with_fd<R>(&mut self, cmd: &Command, fd: Option<&UnixStream>) -> Result<R>
    where
        R: DeserializeOwned,
    {
        let mut text = serde_json::to_string(cmd)?;
        text.push('\n');
        self.send(&text, fd)?;
        loop {
            let line = self.read_line()?.context("QMP connection closed")?;
            let msg: Message<R> = serde_json::from_str(&line)?;
            if msg.event.is_some() {
                self.pending.push_back(serde_json::from_str(&line)?);
            } else if let Some(e) = msg.error {
                let name = serde_json::from_str::<Execute<'_>>(&text).map_or("QMP", |c| c.execute);
                return Err(anyhow::Error::new(e).context(name.to_owned()));
            } else if let Some(ret) = msg.ret {
                return Ok(ret);
            } else {
                bail!("unexpected QMP message: {}", line.trim());
            }
        }
    }

    pub fn execute<R>(&mut self, cmd: &Command) -> Result<R>
    where
        R: DeserializeOwned,
    {
        self.execute_with_fd(cmd, None)
    }

    pub fn run(&mut self, cmd: &Command) -> Result<()> {
        self.execute::<IgnoredAny>(cmd).map(drop)
    }

    // Ok(None) on timeout. Err once QEMU has closed the connection.
    pub fn next_event(&mut self, timeout: Duration) -> Result<Option<Event>> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(Some(event));
        }
        self.stream.set_read_timeout(Some(timeout))?;
        let line = self.read_line();
        self.stream
            .set_read_timeout(Some(Duration::from_secs(10)))?;
        match line {
            Ok(Some(line)) => {
                let msg: Message<IgnoredAny> = serde_json::from_str(&line)?;
                if msg.event.is_some() {
                    Ok(Some(serde_json::from_str(&line)?))
                } else {
                    Ok(None)
                }
            },
            Ok(None) => bail!("QMP connection closed"),
            Err(e) => match e.downcast_ref::<std::io::Error>() {
                Some(io) if matches!(io.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    Ok(None)
                },
                _ => Err(e),
            },
        }
    }
}

// QEMU serves one peer-to-peer D-Bus display client at a time: a new one takes the display.
pub fn attach_display(qmp_path: &Path) -> Result<UnixStream> {
    let mut qmp = Qmp::connect(qmp_path)?;
    let (ours, theirs) = UnixStream::pair()?;
    qmp.execute_with_fd::<IgnoredAny>(&Command::Getfd { fdname: "loophole" }, Some(&theirs))?;
    qmp.run(&Command::AddClient {
        protocol: "@dbus-display",
        fdname: "loophole",
    })
    .context("QEMU has no peer-to-peer D-Bus display (-display dbus,p2p=on)")?;
    Ok(ours)
}

// The guest may hold the tray shut. These force it.
pub fn change_medium(drive: &'static str, media: Option<PathBuf>) -> Vec<Command> {
    let Some(filename) = media else {
        return vec![
            Command::BlockdevOpenTray {
                id: drive,
                force: true,
            },
            Command::BlockdevRemoveMedium { id: drive },
            Command::BlockdevCloseTray { id: drive },
        ];
    };
    vec![Command::BlockdevChangeMedium {
        id: drive,
        filename,
        format: "raw",
        read_only_mode: ReadOnlyMode::ReadOnly,
        force: true,
    }]
}
