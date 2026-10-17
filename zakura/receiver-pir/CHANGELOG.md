# Changelog

## [Unreleased]

### Added

- The crate, moved from wallet-pir's `receiver-pir` at main 6703f263, over `zakura-pir-native`.

### Changed

- `transport::DirectoryClient` reads histories over PIR only, one page per query:
  `DirectoryClient::page` reads one page, and `lookup` reads a whole history page by page with
  no page limit. The row-file download is gone, with `prefer_directory_file`,
  `MAX_PIR_PAGES` and `use_file_for_work`, and `connect_manifest` no longer takes a lookup
  count. A wallet that pages through a long history across revisions keeps its own cursor.
