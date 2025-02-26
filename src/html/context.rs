use crate::html::{self, Anchorizer};
use crate::{Options, Plugins};

use std::cell::Cell;
use std::io::{self, Write};

/// Context struct given to formatter functions as taken by
/// [`html::format_document_with_formatter`].  Output can be appended to through
/// this struct's [`Write`] interface.
pub struct Context<'o, 'c> {
    output: &'o mut dyn Write,
    last_was_lf: Cell<bool>,

    /// [`Options`] in use in this render.
    pub options: &'o Options<'c>,
    /// [`Plugins`] in use in this render.
    pub plugins: &'o Plugins<'o>,
    /// [`Anchorizer`] instance used in this render.
    pub anchorizer: Anchorizer,

    pub(super) footnote_ix: u32,
    pub(super) written_footnote_ix: u32,
}

impl<'o, 'c> Context<'o, 'c> {
    pub(super) fn new(
        output: &'o mut dyn Write,
        options: &'o Options<'c>,
        plugins: &'o Plugins<'o>,
    ) -> Self {
        Context {
            output,
            last_was_lf: Cell::new(true),
            options,
            plugins,
            anchorizer: Anchorizer::new(),
            footnote_ix: 0,
            written_footnote_ix: 0,
        }
    }

    pub(super) fn finish(&mut self) -> io::Result<()> {
        if self.footnote_ix > 0 {
            self.write_all(b"</ol>\n</section>\n")?;
        }
        Ok(())
    }

    /// If the last byte written to ts [`Write`] interface was **not** a U+000A
    /// LINE FEED, writes one. Otherwise, does nothing.
    ///
    /// (In other words, ensures the output is at a new line.)
    pub fn cr(&mut self) -> io::Result<()> {
        if !self.last_was_lf.get() {
            self.write_all(b"\n")?;
        }
        Ok(())
    }

    /// Convenience wrapper for [`html::escape`].
    pub fn escape(&mut self, buffer: &[u8]) -> io::Result<()> {
        html::escape(self, buffer)
    }

    /// Convenience wrapper for [`html::escape_href`].
    pub fn escape_href(&mut self, buffer: &[u8]) -> io::Result<()> {
        html::escape_href(self, buffer)
    }
}

impl<'o, 'c> Write for Context<'o, 'c> {
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let l = buf.len();
        if l > 0 {
            self.last_was_lf.set(buf[l - 1] == 10);
        }
        self.output.write(buf)
    }
}

impl<'o, 'c> std::fmt::Debug for Context<'o, 'c> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        formatter.write_str("<comrak::html::Context>")
    }
}
