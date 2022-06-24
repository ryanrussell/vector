use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use crc32fast::Hasher;
use snafu::Snafu;

use super::{io::{Filesystem, ProductionFilesystem}, record::RECORD_HEADER_LEN};

// We don't want data files to be bigger than 128MB, but we might end up overshooting slightly.
pub const DEFAULT_MAX_DATA_FILE_SIZE: usize = 128 * 1024 * 1024;

// We allow records to be as large as a data file.
//
// Practically, this means we'll allow records that are just about as big as as a single data file, but they won't
// _exceed_ the size of a data file, even if they're the first write to a data file.
pub const DEFAULT_MAX_RECORD_SIZE: usize = DEFAULT_MAX_DATA_FILE_SIZE;

// We want to ensure a reasonable time before we `fsync`/flush to disk, and 500ms should provide that for non-critical
// workloads.
//
// Practically, it's far more definitive than `disk_v1` which does not definitvely `fsync` at all, at least with how we
// have it configured.
pub const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_millis(500);

// Using 256KB as it aligns nicely with the I/O size exposed by major cloud providers.  This may not
// be the underlying block size used by the OS, but it still aligns well with what will happen on
// the "backend" for cloud providers, which is simply a useful default for when we want to look at
// buffer throughput and estimate how many IOPS will be consumed, etc.
pub const DEFAULT_WRITE_BUFFER_SIZE: usize = 256 * 1024;

// We specifically limit ourselves to 0-31 for file IDs in test, because it lets us more quickly
// create/consume the file IDs so we can test edge cases like file ID rollover and "writer is
// waiting to open file that reader is still on".
#[cfg(not(test))]
pub const MAX_FILE_ID: u16 = u16::MAX;
#[cfg(test)]
pub const MAX_FILE_ID: u16 = 6;

pub(crate) fn create_crc32c_hasher() -> Hasher {
    crc32fast::Hasher::new()
}

#[derive(Debug, Snafu)]
pub enum BuildError {
    #[snafu(display("parameter '{}' was invalid: {}", param_name, reason))]
    InvalidParameter {
        param_name: &'static str,
        reason: String,
    },
}

/// Buffer configuration.
#[derive(Clone, Debug)]
pub struct DiskBufferConfig<FS> {
    /// Directory where this buffer will write its files.
    ///
    /// Must be unique from all other buffers, whether within the same process or other Vector
    /// processes on the machine.
    pub(crate) data_dir: PathBuf,

    /// Maximum size, in bytes, that the buffer can consume.
    ///
    /// The actual maximum on-disk buffer size is this amount rounded up to the next multiple of
    /// `max_data_file_size`, but internally, the next multiple of `max_data_file_size` when
    /// rounding this amount _down_ is what gets used as the maximum buffer size.
    ///
    /// This ensures that we never use more then the documented "rounded to the next multiple"
    /// amount, as we must account for one full data file's worth of extra data.
    pub(crate) max_buffer_size: u64,

    /// Maximum size, in bytes, to target for each individual data file.
    ///
    /// This value is not strictly obey because we cannot know ahead of encoding/serializing if the
    /// free space a data file has is enough to hold the write.  In other words, we never attempt to
    /// write to a data file if it is as larger or larger than this value, but may write a record
    /// that causes a data file to exceed this value by as much as `max_record_size`.
    pub(crate) max_data_file_size: u64,

    /// Maximum size, in bytes, of an encoded record.
    ///
    /// Any record which, when encoded, is larger than this amount (with a small caveat, see note)
    /// will not be written to the buffer.
    pub(crate) max_record_size: usize,

    /// Size, in bytes, of the writer's internal buffer.
    ///
    /// This buffer is used to coalesce writes to the underlying data file where possible, which in
    /// turn reduces the number of syscalls needed to issue writes to the underlying data file.
    pub(crate) write_buffer_size: usize,

    /// Flush interval for ledger and data files.
    ///
    /// While data is asynchronously flushed by the OS, and the reader/writer can proceed with a
    /// "hard" flush (aka `fsync`/`fsyncdata`), the flush interval effectively controls the
    /// acceptable window of time for data loss.
    ///
    /// In the event that data had not yet been durably written to disk, and Vector crashed, the
    /// amount of data written since the last flush would be lost.
    pub(crate) flush_interval: Duration,

    /// Filesystem implementation for opening data files.
    ///
    /// We allow parameterizing the filesystem implementation for ease of testing.  The "filesystem"
    /// implementation essentially defines how we open and delete data files, as well as the type of
    /// the data file objects we get when opening a data file.
    pub(crate) filesystem: FS,
}

/// Builder for [`DiskBufferConfig`].
#[derive(Clone, Debug)]
pub struct DiskBufferConfigBuilder<FS = ProductionFilesystem>
where
    FS: Filesystem,
{
    pub(crate) data_dir: PathBuf,
    pub(crate) max_buffer_size: Option<u64>,
    pub(crate) max_data_file_size: Option<u64>,
    pub(crate) max_record_size: Option<usize>,
    pub(crate) write_buffer_size: Option<usize>,
    pub(crate) flush_interval: Option<Duration>,
    pub(crate) filesystem: FS,
}

impl DiskBufferConfigBuilder {
    pub fn from_path<P>(data_dir: P) -> DiskBufferConfigBuilder
    where
        P: AsRef<Path>,
    {
        DiskBufferConfigBuilder {
            data_dir: data_dir.as_ref().to_path_buf(),
            max_buffer_size: None,
            max_data_file_size: None,
            max_record_size: None,
            write_buffer_size: None,
            flush_interval: None,
            filesystem: ProductionFilesystem,
        }
    }
}

impl<FS> DiskBufferConfigBuilder<FS>
where
    FS: Filesystem,
{
    /// Sets the maximum size, in bytes, that the buffer can consume.
    ///
    /// The actual maximum on-disk buffer size is this amount rounded up to the next multiple of
    /// `max_data_file_size`, but internally, the next multiple of `max_data_file_size` when
    /// rounding this amount _down_ is what gets used as the maximum buffer size.
    ///
    /// This ensures that we never use more then the documented "rounded to the next multiple"
    /// amount, as we must account for one full data file's worth of extra data.
    ///
    /// Defaults to `usize::MAX`, or effectively no limit.  Due to the internal design of the
    /// buffer, the effective maximum limit is around `max_data_file_size` * 2^16.
    #[allow(dead_code)]
    pub fn max_buffer_size(mut self, amount: u64) -> Self {
        self.max_buffer_size = Some(amount);
        self
    }

    /// Sets the maximum size, in bytes, to target for each individual data file.
    ///
    /// This value is not strictly obey because we cannot know ahead of encoding/serializing if the
    /// free space a data file has is enough to hold the write.  In other words, we never attempt to
    /// write to a data file if it is as larger or larger than this value, but may write a record
    /// that causes a data file to exceed this value by as much as `max_record_size`.
    ///
    /// Defaults to 128MB.
    #[allow(dead_code)]
    pub fn max_data_file_size(mut self, amount: u64) -> Self {
        self.max_data_file_size = Some(amount);
        self
    }

    /// Sets the maximum size, in bytes, of an encoded record.
    ///
    /// Any record which, when encoded, is larger than this amount (with a small caveat, see note)
    /// will not be written to the buffer.
    ///
    /// Defaults to 128MB.
    #[allow(dead_code)]
    pub fn max_record_size(mut self, amount: usize) -> Self {
        self.max_record_size = Some(amount);
        self
    }

    /// Size, in bytes, of the writer's internal buffer.
    ///
    /// This buffer is used to coalesce writes to the underlying data file where possible, which in
    /// turn reduces the number of syscalls needed to issue writes to the underlying data file.
    ///
    /// Defaults to 256KB.
    #[allow(dead_code)]
    pub fn write_buffer_size(mut self, amount: usize) -> Self {
        self.write_buffer_size = Some(amount);
        self
    }

    /// Sets the flush interval for ledger and data files.
    ///
    /// While data is asynchronously flushed by the OS, and the reader/writer can proceed with a
    /// "hard" flush (aka `fsync`/`fsyncdata`), the flush interval effectively controls the
    /// acceptable window of time for data loss.
    ///
    /// In the event that data had not yet been durably written to disk, and Vector crashed, the
    /// amount of data written since the last flush would be lost.
    ///
    /// Defaults to 500ms.
    #[allow(dead_code)]
    pub fn flush_interval(mut self, interval: Duration) -> Self {
        self.flush_interval = Some(interval);
        self
    }

    /// Filesystem implementation for opening data files.
    ///
    /// We allow parameterizing the filesystem implementation for ease of testing.  The "filesystem"
    /// implementation essentially defines how we open and delete data files, as well as the type of
    /// the data file objects we get when opening a data file.
    ///
    /// Defaults to a Tokio-backed implementation.
    #[allow(dead_code)]
    pub fn filesystem<FS2>(self, filesystem: FS2) -> DiskBufferConfigBuilder<FS2>
    where
        FS2: Filesystem,
    {
        DiskBufferConfigBuilder {
            data_dir: self.data_dir,
            max_buffer_size: self.max_buffer_size,
            max_data_file_size: self.max_data_file_size,
            max_record_size: self.max_record_size,
            write_buffer_size: self.write_buffer_size,
            flush_interval: self.flush_interval,
            filesystem,
        }
    }

    /// Consumes this builder and constructs a `DiskBufferConfig`.
    pub fn build(self) -> Result<DiskBufferConfig<FS>, BuildError> {
        let max_buffer_size = self.max_buffer_size.unwrap_or(u64::MAX);
        let max_data_file_size = self.max_data_file_size.unwrap_or_else(|| {
            u64::try_from(DEFAULT_MAX_DATA_FILE_SIZE)
                .expect("Vector does not support 128-bit platforms.")
        });
        let max_record_size = self.max_record_size.unwrap_or(DEFAULT_MAX_RECORD_SIZE);
        let write_buffer_size = self.write_buffer_size.unwrap_or(DEFAULT_WRITE_BUFFER_SIZE);
        let flush_interval = self.flush_interval.unwrap_or(DEFAULT_FLUSH_INTERVAL);
        let filesystem = self.filesystem;

        // Validate the input parameters.
        if max_data_file_size == 0 {
            return Err(BuildError::InvalidParameter {
                param_name: "max_data_file_size",
                reason: "cannot be zero".to_string(),
            });
        }

        if max_data_file_size > u64::MAX / 2 {
            return Err(BuildError::InvalidParameter {
                param_name: "max_data_file_size",
                reason: format!("cannot be greater than {} bytes", u64::MAX / 2),
            });
        }

        if max_buffer_size < max_data_file_size * 2 {
            return Err(BuildError::InvalidParameter {
                param_name: "max_buffer_size",
                reason: format!(
                    "must be greater than or equal to {} bytes",
                    max_data_file_size * 2
                ),
            });
        }

        if max_record_size == 0 {
            return Err(BuildError::InvalidParameter {
                param_name: "max_record_size",
                reason: "cannot be zero".to_string(),
            });
        }

        if max_record_size <= RECORD_HEADER_LEN {
            return Err(BuildError::InvalidParameter {
                param_name: "max_record_size",
                reason: format!(
                    "must be greater than {} bytes",
                    RECORD_HEADER_LEN,
                ),
            });
        }

        let max_record_size_converted = u64::try_from(max_record_size)
            .expect("Vector only supports 64-bit architectures.");
        if max_record_size_converted > max_data_file_size {
            return Err(BuildError::InvalidParameter {
                param_name: "max_record_size",
                reason:  "must be less than or equal to `max_data_file_size`".to_string(),
            });
        }

        if write_buffer_size == 0 {
            return Err(BuildError::InvalidParameter {
                param_name: "write_buffer_size",
                reason: "cannot be zero".to_string(),
            });
        }

        // Users configure the `max_size` of their disk buffers, which translates to the `max_buffer_size` field here,
        // and represents the maximum desired size of a disk buffer in terms of on-disk usage. In order to meet this
        // request, we do a few things internally and also enforce a lower bound on `max_buffer_size` to ensure we can
        // commit to respecting the communicated maximum buffer size.
        //
        // Internally, we track the current buffer size as a function of the sum of the size of all unacknowledged
        // records.  This means, simply, that if 100 records are written that consume 1KB a piece, our current buffer
        // size should be around 100KB, and as those records are read and acknowledged, the current buffer size would
        // drop by 1KB for each of them until eventually it went back down to zero.
        //
        // One of the design invariants around data files is that they are written to until they reach the maximum data
        // file size, such that they are guaranteed to never be greater in size than `max_data_file_size`. This is
        // coupled with the fact that a data file cannot be deleted from disk until all records written to it have been
        // read _and_ acknowledged.
        //
        // Together, this means that we need to set a lower bound of 2*`max_data_file_size` for `max_buffer_size`.
        //
        // First, given the "data file keeps getting written to until we reach its max size" invariant, we know that in
        // order to commit to the on-disk buffer size not exceeding `max_buffer_size`, the value must be at least as
        // much as a single full data file, aka `max_data_file_size`.
        //
        // Secondly, we also want to ensure that the writer can make progress as the reader makes progress. If the
        // maximum buffer size was equal to the maximum data file size, the writer would be stalled as soon as the data
        // file reached the maximum size, until the reader was able to fully read and acknowledge all records, and thus
        // delete the data file from disk. If we instead require that the maximum buffer size exceeds
        // `max_data_file_size`, this allows us to open the next data file and start writing to it up until the maximum
        // buffer size.
        //
        // Since we could essentially read and acknowledge all but the last remaining record in a data file, this would
        // imply we gave the writer the ability to write that much more data, which means we would need at least double
        // the maximum data file size in order to support the writer being able to make progress in the aforementioned
        // situation.
        //
        // Finally, we come to this calculation. Since the logic dictates that we essentially require at least one extra
        // data file past the minimum of one, we need to use an _internal_ maximum buffer size of `max_buffer_size` -
        // `max_data_file_size`, so that as the reader makes progress, the writer never is led to believe it can create
        // another data file such that the number of active data files, multiplied by `max_data_file_size`, would exceed
        // `max_buffer_size`.
        let max_buffer_size = max_buffer_size - max_data_file_size;

        Ok(DiskBufferConfig {
            data_dir: self.data_dir,
            max_buffer_size,
            max_data_file_size,
            max_record_size,
            write_buffer_size,
            flush_interval,
            filesystem,
        })
    }
}

#[cfg(test)]
mod tests {
    use proptest::{prop_assert, proptest, test_runner::Config};

    use crate::variants::disk_v2::{DiskBufferConfigBuilder, record::RECORD_HEADER_LEN};

    use super::BuildError;

    #[test]
    fn basic_rejections() {
        // Maximum data file size cannot be zero.
        let result = DiskBufferConfigBuilder::from_path("/tmp/dummy/path")
            .max_data_file_size(0)
            .build();

        match result {
            Err(BuildError::InvalidParameter { param_name, .. }) => assert_eq!(param_name, "max_data_file_size", "invalid parameter should have been `max_data_file_size`"),
            _ => panic!("expected invalid parameter error"),
        }

        // Maximum data file size cannot be greater than u64::MAX / 2, since we multiply it by 2 when calculating the
        // lower bound for the maximum buffer size.
        let result = DiskBufferConfigBuilder::from_path("/tmp/dummy/path")
            .max_data_file_size((u64::MAX / 2) + 1)
            .build();

        match result {
            Err(BuildError::InvalidParameter { param_name, .. }) => assert_eq!(param_name, "max_data_file_size", "invalid parameter should have been `max_data_file_size`"),
            _ => panic!("expected invalid parameter error"),
        }

        // Maximum buffer size cannot be zero.
        let result = DiskBufferConfigBuilder::from_path("/tmp/dummy/path")
            .max_buffer_size(0)
            .build();

        match result {
            Err(BuildError::InvalidParameter { param_name, .. }) => assert_eq!(param_name, "max_buffer_size", "invalid parameter should have been `max_buffer_size`"),
            _ => panic!("expected invalid parameter error"),
        }

        // Maximum buffer size cannot be less than 2x the maximum data file size.
        let result = DiskBufferConfigBuilder::from_path("/tmp/dummy/path")
            .max_data_file_size(10000)
            .max_record_size(100)
            .max_buffer_size(19999)
            .build();

        match result {
            Err(BuildError::InvalidParameter { param_name, .. }) => assert_eq!(param_name, "max_buffer_size", "invalid parameter should have been `max_buffer_size`"),
            _ => panic!("expected invalid parameter error"),
        }

        // Maximum record size cannot be zero.
        let result = DiskBufferConfigBuilder::from_path("/tmp/dummy/path")
            .max_record_size(0)
            .build();

        match result {
            Err(BuildError::InvalidParameter { param_name, .. }) => assert_eq!(param_name, "max_record_size", "invalid parameter should have been `max_record_size`"),
            _ => panic!("expected invalid parameter error"),
        }

        // Maximum record size cannot be less than or equal to the record header length.
        let result = DiskBufferConfigBuilder::from_path("/tmp/dummy/path")
            .max_record_size(RECORD_HEADER_LEN)
            .build();

        match result {
            Err(BuildError::InvalidParameter { param_name, .. }) => assert_eq!(param_name, "max_record_size", "invalid parameter should have been `max_record_size`"),
            _ => panic!("expected invalid parameter error"),
        }

        // Maximum record size cannot be greater than maximum data file size.
        let result = DiskBufferConfigBuilder::from_path("/tmp/dummy/path")
            .max_data_file_size(123456)
            .max_record_size(123457)
            .build();

        match result {
            Err(BuildError::InvalidParameter { param_name, .. }) => assert_eq!(param_name, "max_record_size", "invalid parameter should have been `max_record_size`"),
            _ => panic!("expected invalid parameter error"),
        }
    }

    proptest! {
        #![proptest_config(Config::with_cases(10000))]
        #[test]
        fn ensure_max_buffer_size_lower_bound(max_buffer_size in 1..u64::MAX, max_record_data_file_size in 1..u64::MAX) {
            let max_data_file_size = max_record_data_file_size;
            let max_record_size = usize::try_from(max_record_data_file_size)
                .expect("Vector only supports 64-bit architectures.");

            let result = DiskBufferConfigBuilder::from_path("/tmp/dummy/path")
                .max_buffer_size(max_buffer_size)
                .max_data_file_size(max_data_file_size)
                .max_record_size(max_record_size)
                .build();

            // We don't necessarily care about the error cases here, but what we do care about is making sure that, when
            // the generated configuration is theoretically valid, the calculated maximum buffer size actually meets our expectation of
            // being at least `max_data_file_size` and `max_data_file_size` less than the input maximum buffer size.
            if let Ok(config) = result {
                prop_assert!(config.max_buffer_size >= max_data_file_size, "calculated max buffer size must always be greater than or equal to `max_data_file_size`");
                prop_assert!(config.max_buffer_size + max_data_file_size == max_buffer_size, "calculated max buffer size must always be less `max_data_file_size` than input max buffer size");
            }
        }
    }
}
