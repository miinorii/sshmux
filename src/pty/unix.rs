//! Thin adapter around `portable_pty` for Unix targets.

use std::io::{Read, Write};

use anyhow::Result;
use portable_pty::{
    Child as PpChild, MasterPty as PpMaster, PtySize, SlavePty as PpSlave, native_pty_system,
};

use super::CommandBuilder;

pub struct PtyPair {
    pub master: PtyMaster,
    pub slave: PtySlave,
}

pub struct PtyMaster {
    inner: Box<dyn PpMaster + Send>,
}

impl PtyMaster {
    pub fn take_writer(&self) -> Result<Box<dyn Write + Send>> {
        Ok(self.inner.take_writer()?)
    }

    pub fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>> {
        Ok(self.inner.try_clone_reader()?)
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.inner.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }
}

pub struct PtySlave {
    inner: Box<dyn PpSlave>,
}

impl PtySlave {
    pub fn spawn_command(&self, cmd: CommandBuilder) -> Result<PtyChild> {
        let child = self.inner.spawn_command(cmd)?;
        Ok(PtyChild { inner: child })
    }
}

pub struct PtyChild {
    inner: Box<dyn PpChild + Send + Sync>,
}

impl PtyChild {
    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.inner
            .try_wait()
            .map(|o| o.map(|s| ExitStatus { code: s.exit_code() }))
    }

    pub fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.inner.wait().map(|s| ExitStatus { code: s.exit_code() })
    }

    pub fn kill(&mut self) -> std::io::Result<()> {
        self.inner.kill()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ExitStatus {
    pub code: u32,
}

pub fn openpty(rows: u16, cols: u16) -> Result<PtyPair> {
    let pair = native_pty_system().openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    Ok(PtyPair {
        master: PtyMaster { inner: pair.master },
        slave: PtySlave { inner: pair.slave },
    })
}
