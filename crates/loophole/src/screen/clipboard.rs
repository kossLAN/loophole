use std::io::{self, BufReader};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use loophole_vm::clipboard;

const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

type Job = Box<dyn FnOnce(&mut arboard::Clipboard) + Send>;

fn host(job: impl FnOnce(&mut arboard::Clipboard) + Send + 'static) {
    static HOST: OnceLock<Option<Sender<Job>>> = OnceLock::new();

    let tx = HOST.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();

        std::thread::Builder::new()
            .name("host clipboard".into())
            .spawn(move || {
                let mut clipboard = match arboard::Clipboard::new() {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("loophole: no host clipboard: {e}");
                        return;
                    },
                };

                for job in rx {
                    job(&mut clipboard);
                }
            })
            .ok()
            .map(|_| tx)
    });

    if let Some(tx) = tx {
        let _ = tx.send(Box::new(job));
    }
}

struct Shared {
    stream: UnixStream,
    last: Mutex<Option<String>>,
}

impl Shared {
    fn last(&self) -> MutexGuard<'_, Option<String>> {
        self.last.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn guest_to_host(&self, reader: UnixStream) {
        let mut reader = BufReader::new(reader);

        loop {
            let text = match clipboard::read_text(&mut reader) {
                Ok(Some(text)) => text,
                Ok(None) => return,
                Err(e) => {
                    eprintln!("loophole: clipboard: {e}");
                    return;
                },
            };

            if self.last().replace(text.clone()).as_ref() == Some(&text) {
                continue;
            }

            host(move |clipboard| {
                if let Err(e) = clipboard.set_text(text) {
                    eprintln!("loophole: clipboard: {e}");
                }
            });
        }
    }

    fn host_to_guest(&self, text: String) {
        let mut last = self.last();

        if last.as_ref() == Some(&text) {
            return;
        }

        match clipboard::write_text(&mut &self.stream, &text) {
            Ok(()) => *last = Some(text),
            Err(e) => eprintln!("loophole: clipboard: {e}"),
        }
    }
}

pub struct Link(Arc<Shared>);

impl Link {
    pub fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        let reader = stream.try_clone()?;
        let shared = Arc::new(Shared {
            stream,
            last: Mutex::new(None),
        });
        let s = shared.clone();
        std::thread::Builder::new()
            .name("guest clipboard".into())
            .spawn(move || s.guest_to_host(reader))?;

        Ok(Self(shared))
    }

    pub fn push_host(&self) {
        let s = self.0.clone();
        host(move |clipboard| match clipboard.get_text() {
            Ok(text) => s.host_to_guest(text),
            Err(arboard::Error::ContentNotAvailable) => {},
            Err(e) => eprintln!("loophole: clipboard: {e}"),
        });
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.0.stream.shutdown(Shutdown::Both);
    }
}
