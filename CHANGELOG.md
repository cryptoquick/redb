# redb - Changelog

## 0.13.0 - 2023-02-05
* Fix a major data corruption issue that was introduced in version 0.12.0. It caused databases
  greater than ~4GB to become irrecoverably corrupted due to an integer overflow in `PageNumber::address_range`
  that was introduced by commit `b2c44a824d1ba69f526a1a75c56ae8484bae7248`
* Add `drain_filter()` to `Table`
* Make key and value type bounds more clear for tables

## 0.12.1 - 2023-01-22
* Fix `open()` on platforms with OS page size != 4KiB
* Relax lifetime requirements on argument to `range()` and `drain()`

## 0.12.0 - 2023-01-21
* Add `pop_first()` and `pop_last()` to `Table`
* Add `drain()` to `Table`
* Add support for `Option<T>` as a value type
* Add support for user defined key and value types. Users must implement `RedbKey` and/or `RedbValue`
* Change `get()`, `insert()`, `remove()`...etc to take arguments of type `impl Borrow<SelfType>`
* Return `Error::UpgradeRequired` when opening a file with an outdated file format
* Improve support for 32bit platforms
* Performance optimizations

## 0.11.0 - 2022-12-26
* Remove `[u8]` and `str` type support. Use `&[u8]` and `&str` instead.
* Change `get()`, `range()` and several other methods to return `AccessGuard`.
* Rename `AccessGuard::to_value()` to `value()`
* Add a non-mmap based backend which is now the default. This makes `Database::create()` and
  `Database::open()` safe, but has worse performance in some cases. The mmap backend is available
  via `create_mmapped()`/`open_mmapped()`. There is no difference in the file format, so applications
  can switch from one backend to the other.
* Better handling of fsync failures

## 0.10.0 - 2022-11-23
* Remove maximum database size argument from `create()`. Databases are now unbounded in size
* Reduce address space usage on Windows
* Remove `set_dynamic_growth()`
* Add `set_initial_size()` to `Builder`
* Optimize cleanup of deleted pages. This resolves a performance issue where openning a Database
  or performing a small transaction, could be slow if the last committed transaction deleted a large
  number of pages
* Remove `set_page_size()`. 4kB pages are always used now
* Add `iter()` method to `Table` and `MultimapTable`
* Fix various lifetime issues with type that had a lifetime, such as `&str` and `(&[u8], u64)`

## 0.9.0 - 2022-11-05
* Add support for dynamic file growth on Windows
* Add support for tuple types as keys and values
* Remove `Builder::set_region_size`
* Save lifetime from `Savepoint`
* Fix crash when using `create()` to open an existing database created with `WriteStrategy::TwoPhase`
* Fix rare crash when writing a mix of small and very large values into the same table
* Performance optimizations

## 0.8.0 - 2022-10-18
* Performance improvements for database files that are too large to fit in RAM
* Fix deadlock in concurrent calls to `savepoint()` and `restore_savepoint()`
* Fix crash if `restore_savepoint()` failed
* Move `savepoint()` and `restore_savepoint()` methods to `WriteTransaction`
* Implement `Iterator` for the types returned from `range()` and `remove_all()`

## 0.7.0 - 2022-09-25
* Add support for Windows
* Add `Database::set_write_strategy` which allows the `WriteStrategy` of the database to be changed after creation
* Make `Database::begin_write` block, instead of panic'ing, if there is another write already in progress
* Add `Database::savepoint` and `Database::restore_savepoint` which can be used to snapshot and rollback the database
* Rename `DatabaseBuilder` to `Builder`
* Performance optimizations for large databases

## 0.6.1 - 2022-09-11
* Fix crash when `Database::open()` was called on a database that had been created with `WriteStrategy::TwoPhase`
* Change default region size on 32bit platforms to 4GiB

## 0.6.0 - 2022-09-10
* Return `Err` instead of panic'ing when opening a database file with an incompatible file format version
* Many optimizations to the file format, and progress toward stabilizing it
* Fix race between read & write transactions, which could cause reads to return corrupted data
* Better document the different `WriteStrategy`s
* Fix panic when recovering a database that was uncleanly shutdown, which had been created with `WriteStrategy::Checksum` (which is the default)
* Fix panic when using `insert_reserve()` in certain cases

## 0.5.0 - 2022-08-06
* Optimize `MultimapTable` storage format to use `O(k * log(n_k) + v * log(n_v / n_k))` space instead of `O(k * log(n_k + n_v) + v * log(n_k + n_v))` space,
  where k is the size of the stored keys, v is the size of the stored values, n_k is the number of stored keys,
  n_v is the number of stored values
* Fix compilation errors for 32bit x86 targets
* Add support for the unit type, `()`, as a value
* Return an error when attempting to open the same database file for writing in multiple locations, concurrently
* More robust handling of fsync failures
* Change `MultimapTable::range` to return an iterator of key-value-collection pairs, instead of key-value pairs
* Automatically abort `WriteTransaction` on drop

## 0.4.0 - 2022-07-26
* Add single phase with checksum commit strategy. This is now the default and reduces commit latency by ~2x. For more details,
  see the [design doc](docs/design.md#1-phase--checksum-durable-commits) and
  [blog post](https://www.redb.org/post/2022/07/26/faster-commits-with-1pcc-instead-of-2pc/). The previous behavior is available
  via `WriteStrategy::Throughput`, and can have better performance when writing a large number of bytes per transaction.

## 0.3.1 - 2022-07-20
* Fix a bug where re-opening a `Table` during a `WriteTransaction` lead to stale results being read

## 0.3.0 - 2022-07-19
* Fix a serious data corruption issue that caused many write operations to corrupt the database
* Make redb region size configurable
* Implement garbage collection of empty regions
* Fixes and optimizations to make the file format more efficient

## 0.2.0 - 2022-06-10
* Add information log messages which can be enabled with the `logging` feature
* Add support for `[u8; N]` type
* Optimize storage of fixed width types. The value length is no longer stored, which reduces storage space by ~50% for `u64`,
  2x for `u32`, and also improves performance.

## 0.1.2 - 2022-05-08
* Change `insert()` to return an `Option<V>` with the previous value, instead of `()`

## 0.1.1 - 2022-04-24
* Improved documentation

## 0.1.0 - 2022-04-23
* Initial beta release
