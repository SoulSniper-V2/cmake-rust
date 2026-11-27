# file(ARCHIVE_CREATE) compresses with one thread (cmArchiveWrite default),
# so the xz stream is byte-identical to the reference.
file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/in/a.txt" "data data data data\n")
file(ARCHIVE_CREATE OUTPUT "${CMAKE_CURRENT_BINARY_DIR}/a.tar.xz"
  PATHS "${CMAKE_CURRENT_BINARY_DIR}/in/a.txt"
  FORMAT gnutar COMPRESSION XZ MTIME "2020-01-01 00:00:00 UTC")
file(SHA256 "${CMAKE_CURRENT_BINARY_DIR}/a.tar.xz" h)
message("${h}")
