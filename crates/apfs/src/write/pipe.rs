//! Commits in two steps, so changes keep coming while the disk catches up.
//! [`super::Writer::prepare`] builds every block of a transaction in memory
//! (quick, done under the caller's lock); [`Prepared::persist`] writes them
//! (slow on a USB drive, done without it). Until a commit is on the disk its
//! blocks are served from memory, so readers and the next transaction see
//! the newest state; blocks it freed stay unused until it lands (see
//! [`Pipe::durable`]), so a crash at any moment leaves the last commit on
//! the disk intact.
//!
//! File data goes out the same way: [`DataDev`] hands each buffer to
//! background writers (several disk handles, so the disk has more than one
//! request to work on) and returns at once; readers see buffers in flight,
//! and a commit waits for the data it points to before it writes anything.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::Instant;

use crate::dev::{BlockDevice, WriteDevice};
use crate::error::{corrupt, Result};

/// File data allowed in flight at once.
const MAX_IN_FLIGHT: usize = 32 << 20;

pub(crate) struct Pipe {
    pub(crate) dev: Arc<dyn WriteDevice>,
    bs: u64,
    /// Blocks of commits not yet on the disk: block -> (xid, contents).
    pending: Mutex<HashMap<u64, (u64, Arc<[u8]>)>>,
    /// The newest transaction on the disk. Commits land in order.
    durable: Mutex<u64>,
    landed: Condvar,
    /// A commit did not land; later ones build on it, so none may.
    broken: AtomicBool,
    /// File data on its way to the disk.
    data: Mutex<Flights>,
    data_done: Condvar,
    jobs: Mutex<Option<mpsc::Sender<(u64, Arc<Vec<u8>>, u64)>>>,
}

/// File data in flight. Sequence numbers say what was handed over before what.
#[derive(Default)]
struct Flights {
    /// (byte offset, contents, sequence number).
    flying: Vec<(u64, Arc<Vec<u8>>, u64)>,
    bytes: usize,
    next_seq: u64,
    error: Option<String>,
}

impl Pipe {
    /// A pipe writing commits through `dev`, and file data through
    /// `writers` (extra handles on the same disk) in the background.
    pub(crate) fn new(dev: Arc<dyn WriteDevice>, writers: Vec<Box<dyn WriteDevice>>, bs: u64, durable: u64) -> Arc<Self> {
        let pipe = Arc::new(Self {
            dev,
            bs,
            pending: Mutex::default(),
            durable: Mutex::new(durable),
            landed: Condvar::new(),
            broken: AtomicBool::new(false),
            data: Mutex::default(),
            data_done: Condvar::new(),
            jobs: Mutex::new(None),
        });
        if !writers.is_empty() {
            let (tx, rx) = mpsc::channel::<(u64, Arc<Vec<u8>>, u64)>();
            let rx = Arc::new(Mutex::new(rx));
            for w in writers {
                let (rx, p) = (Arc::clone(&rx), Arc::downgrade(&pipe));
                std::thread::spawn(move || loop {
                    let job = rx.lock().unwrap().recv();
                    let Ok((off, buf, seq)) = job else { break };
                    let r = w.write_at(off, &buf);
                    let Some(p) = p.upgrade() else { break };
                    p.landed_data(seq, buf.len(), r.err().map(|e| e.to_string()));
                });
            }
            *pipe.jobs.lock().unwrap() = Some(tx);
        }
        pipe
    }

    /// Hands `buf` (for byte offset `off`) to the background writers. Waits
    /// first while an earlier buffer for the same bytes is still flying (so
    /// an older copy cannot land after a newer one), or while too much is.
    fn submit(&self, off: u64, buf: Vec<u8>) -> Result<()> {
        let end = off + buf.len() as u64;
        let mut d = self.data.lock().unwrap();
        loop {
            if let Some(e) = &d.error {
                return Err(corrupt(format!("writing file data failed: {e}")));
            }
            let clash = d.flying.iter().any(|(o, b, _)| *o < end && off < o + b.len() as u64);
            if !clash && (d.bytes == 0 || d.bytes + buf.len() <= MAX_IN_FLIGHT) {
                break;
            }
            d = self.data_done.wait(d).unwrap();
        }
        let jobs = self.jobs.lock().unwrap();
        let Some(tx) = jobs.as_ref() else {
            drop(d);
            return self.dev.write_at(off, &buf);
        };
        let (seq, buf) = (d.next_seq, Arc::new(buf));
        d.next_seq += 1;
        d.bytes += buf.len();
        d.flying.push((off, Arc::clone(&buf), seq));
        drop(d);
        tx.send((off, buf, seq)).map_err(|_| corrupt("the background writers stopped"))
    }

    fn landed_data(&self, seq: u64, len: usize, error: Option<String>) {
        let mut d = self.data.lock().unwrap();
        d.flying.retain(|(_, _, s)| *s != seq);
        d.bytes -= len;
        if error.is_some() && d.error.is_none() {
            d.error = error;
        }
        drop(d);
        self.data_done.notify_all();
    }

    /// The sequence number the next file-data buffer will get: everything
    /// handed over so far is below it.
    pub(crate) fn data_mark(&self) -> u64 {
        self.data.lock().unwrap().next_seq
    }

    /// Waits until every file-data buffer handed over before `mark` is on
    /// the disk (written, not yet flushed).
    pub(crate) fn wait_data(&self, mark: u64) -> Result<()> {
        let mut d = self.data.lock().unwrap();
        loop {
            if let Some(e) = &d.error {
                return Err(corrupt(format!("writing file data failed: {e}")));
            }
            if !d.flying.iter().any(|(_, _, s)| *s < mark) {
                return Ok(());
            }
            d = self.data_done.wait(d).unwrap();
        }
    }

    /// The newest transaction on the disk. Blocks freed by later ones are
    /// still used by it, so they must not be handed out yet.
    pub(crate) fn durable(&self) -> u64 {
        *self.durable.lock().unwrap()
    }

    pub(crate) fn is_broken(&self) -> bool {
        self.broken.load(Ordering::SeqCst)
    }

    pub(crate) fn break_off(&self) {
        self.broken.store(true, Ordering::SeqCst);
        self.landed.notify_all();
    }

    /// Makes `blocks` (runs starting at a block address) visible to readers.
    pub(crate) fn stage(&self, xid: u64, blocks: &[(u64, Vec<u8>)]) {
        let mut pending = self.pending.lock().unwrap();
        for (start, data) in blocks {
            for (i, b) in data.chunks(self.bs as usize).enumerate() {
                pending.insert(start + i as u64, (xid, b.into()));
            }
        }
    }

    /// Reads through to the disk, with blocks not yet written served from memory.
    pub(crate) fn reader(self: &Arc<Self>, base: Box<dyn BlockDevice>) -> Box<dyn BlockDevice> {
        Box::new(PipeReader { base, pipe: Arc::clone(self) })
    }
}

struct PipeReader {
    base: Box<dyn BlockDevice>,
    pipe: Arc<Pipe>,
}

impl BlockDevice for PipeReader {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<()> {
        self.pipe.read_through(&*self.base, off, buf)
    }
}

impl Pipe {
    /// Reads from `base`, with whatever is still on its way to it served
    /// from memory.
    fn read_through(&self, base: &dyn BlockDevice, off: u64, buf: &mut [u8]) -> Result<()> {
        let bs = self.bs;
        let end = off + buf.len() as u64;
        // Take what is in flight first: it may land (and leave the maps)
        // while the disk is read, and the copies here are still right.
        let flights: Vec<(u64, Arc<Vec<u8>>)> = {
            let d = self.data.lock().unwrap();
            d.flying.iter().filter(|(o, b, _)| *o < end && off < o + b.len() as u64).map(|(o, b, _)| (*o, Arc::clone(b))).collect()
        };
        let hits: Vec<(u64, Arc<[u8]>)> = {
            let pending = self.pending.lock().unwrap();
            if pending.is_empty() {
                Vec::new()
            } else {
                let (first, last) = (off / bs, end.div_ceil(bs));
                if last - first <= pending.len() as u64 {
                    (first..last).filter_map(|b| pending.get(&b).map(|(_, d)| (b, Arc::clone(d)))).collect()
                } else {
                    pending.iter().filter(|(b, _)| (first..last).contains(b)).map(|(b, (_, d))| (*b, Arc::clone(d))).collect()
                }
            }
        };
        base.read_at(off, buf)?;
        for (o, data) in flights {
            let (s, e) = (o.max(off), (o + data.len() as u64).min(end));
            buf[(s - off) as usize..(e - off) as usize].copy_from_slice(&data[(s - o) as usize..(e - o) as usize]);
        }
        for (b, data) in hits {
            let (s, e) = ((b * bs).max(off), ((b + 1) * bs).min(end));
            buf[(s - off) as usize..(e - off) as usize].copy_from_slice(&data[(s - b * bs) as usize..(e - b * bs) as usize]);
        }
        Ok(())
    }
}

/// Where file data is written: in the background (see [`Pipe::submit`]);
/// a flush waits for all of it.
pub(crate) struct DataDev(pub(crate) Arc<Pipe>);

impl BlockDevice for DataDev {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<()> {
        self.0.read_through(&*self.0.dev, off, buf)
    }
}

impl WriteDevice for DataDev {
    fn write_at(&self, off: u64, buf: &[u8]) -> Result<()> {
        self.0.submit(off, buf.to_vec())
    }

    fn flush(&self) -> Result<()> {
        self.0.wait_data(self.0.data_mark())?;
        self.0.dev.flush()
    }
}

/// A transaction built in memory, waiting to be written.
pub struct Prepared {
    pub(crate) pipe: Arc<Pipe>,
    pub(crate) xid: u64,
    /// Its blocks, merged into runs: new objects, ephemeral objects, the
    /// checkpoint map. Everything the superblock points to.
    pub(crate) runs: Vec<(u64, Vec<u8>)>,
    /// The new container superblock and its place in the checkpoint area.
    pub(crate) superblock: (u64, Vec<u8>),
    /// File data handed over before this mark is the transaction's own.
    pub(crate) data_mark: u64,
    pub(crate) done: bool,
}

impl Prepared {
    pub fn xid(&self) -> u64 {
        self.xid
    }

    /// Writes the transaction: its blocks, a flush, both copies of the
    /// superblock, a flush. Waits for earlier transactions to land first.
    pub fn persist(mut self) -> Result<()> {
        self.done = true;
        let pipe = Arc::clone(&self.pipe);
        {
            let mut durable = pipe.durable.lock().unwrap();
            while *durable + 1 < self.xid && !pipe.is_broken() {
                durable = pipe.landed.wait(durable).unwrap();
            }
            if pipe.is_broken() {
                return Err(corrupt(format!("transaction {} builds on one that could not be saved", self.xid)));
            }
            if *durable + 1 != self.xid {
                pipe.break_off();
                return Err(corrupt(format!("transaction {} is out of order (the disk has {})", self.xid, *durable)));
            }
        }
        let start = Instant::now();
        let r = (|| -> Result<f64> {
            // The file data this transaction points to goes first.
            pipe.wait_data(self.data_mark)?;
            let bs = pipe.bs;
            for (block, data) in &self.runs {
                pipe.dev.write_at(block * bs, data)?;
            }
            let t = Instant::now();
            pipe.dev.flush()?;
            let first = t.elapsed().as_secs_f64();
            let (desc, sb) = &self.superblock;
            pipe.dev.write_at(desc * bs, sb)?;
            pipe.dev.write_at(0, sb)?;
            pipe.dev.flush()?;
            Ok(first)
        })();
        match r {
            Ok(first) => {
                *pipe.durable.lock().unwrap() = self.xid;
                pipe.pending.lock().unwrap().retain(|_, (x, _)| *x > self.xid);
                pipe.landed.notify_all();
                let took = start.elapsed().as_secs_f64();
                if took > 1.0 || super::tracing() {
                    super::log(&format!("winapfs: commit {} written in {took:.2} s (first flush {first:.2})", self.xid));
                }
                Ok(())
            }
            Err(e) => {
                pipe.break_off();
                Err(e)
            }
        }
    }
}

impl Drop for Prepared {
    /// A prepared transaction that is never written leaves the ones after it
    /// nothing to build on.
    fn drop(&mut self) {
        if !self.done {
            self.pipe.break_off();
        }
    }
}
