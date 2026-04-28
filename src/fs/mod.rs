mod root;

use std::ffi::OsStr;
use std::sync::Arc;
use std::time::Duration;

use fuser::{FileType, Filesystem, ReplyAttr, ReplyDirectory, ReplyEntry, Request};
use libc::ENOENT;

use crate::cache::Cache;
use crate::client::BtcClient;
use root::{root_attr, ROOT_INO};

const ATTR_TTL: Duration = Duration::from_secs(1);

pub struct BtcFs {
    #[allow(dead_code)]
    client: Arc<dyn BtcClient>,
    #[allow(dead_code)]
    cache: Arc<Cache>,
}

impl BtcFs {
    pub fn new(client: Arc<dyn BtcClient>, cache: Arc<Cache>) -> Self {
        Self { client, cache }
    }
}

impl Filesystem for BtcFs {
    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        if ino == ROOT_INO {
            reply.attr(&ATTR_TTL, &root_attr());
        } else {
            reply.error(ENOENT);
        }
    }

    fn lookup(&mut self, _req: &Request<'_>, _parent: u64, _name: &OsStr, reply: ReplyEntry) {
        reply.error(ENOENT);
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        if ino != ROOT_INO {
            reply.error(ENOENT);
            return;
        }
        let entries = [
            (ROOT_INO, FileType::Directory, "."),
            (ROOT_INO, FileType::Directory, ".."),
        ];
        let skip = usize::try_from(offset).unwrap_or(0);
        for (i, (entry_ino, kind, name)) in entries.into_iter().enumerate().skip(skip) {
            let next_offset = i64::try_from(i + 1).unwrap_or(i64::MAX);
            if reply.add(entry_ino, next_offset, kind, name) {
                break;
            }
        }
        reply.ok();
    }
}
