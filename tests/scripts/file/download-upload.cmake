# file(DOWNLOAD/UPLOAD) over file:// URLs (cmFileCommand on libcurl).
file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/in dir/source.txt" "0123456789abcdefghijklmnopqrstuvwxyz\n")
set(url "file://${CMAKE_CURRENT_BINARY_DIR}/in dir/source.txt")

file(DOWNLOAD "${url}" "${CMAKE_CURRENT_BINARY_DIR}/out/a/copy.txt" STATUS s1)
file(READ "${CMAKE_CURRENT_BINARY_DIR}/out/a/copy.txt" c1)
message("plain: [${s1}] [${c1}]")

file(DOWNLOAD "${url}" "${CMAKE_CURRENT_BINARY_DIR}/out/range.txt" STATUS s2
  RANGE_START 3 RANGE_END 8)
file(READ "${CMAKE_CURRENT_BINARY_DIR}/out/range.txt" c2)
message("range: [${s2}] [${c2}]")

file(DOWNLOAD "${url}" "${CMAKE_CURRENT_BINARY_DIR}/out/a/copy.txt" STATUS s3
  EXPECTED_HASH SHA256=41FF0DAE5AF47B9378835CCEDB25E58D47B1B25F29D77761B2566496A7FC1184)
message("skip: [${s3}]")

file(DOWNLOAD "${url}_missing" STATUS s4 LOG l4)
message("missing: [${s4}]")

file(DOWNLOAD "${url}" STATUS s5 TLS_VERSION 1.3 TLS_VERIFY OFF SHOW_PROGRESS)
message("no-file: [${s5}]")

file(UPLOAD "${CMAKE_CURRENT_BINARY_DIR}/in dir/source.txt"
  "file://${CMAKE_CURRENT_BINARY_DIR}/out/uploaded.txt" STATUS s6 EXTRA)
file(READ "${CMAKE_CURRENT_BINARY_DIR}/out/uploaded.txt" c6)
message("upload: [${s6}] [${c6}]")

file(UPLOAD "${CMAKE_CURRENT_BINARY_DIR}/nope.txt" "file:///dev/null")
file(DOWNLOAD "${url}" "${CMAKE_CURRENT_BINARY_DIR}/out/a/copy.txt" STATUS s3
  EXPECTED_HASH SHA256=1A1C7CF5D3BDE5E3AF3D3EFB7C1E9D1D6D8F5D7E0C1B3F8E9E4A1D5B2C3F4E5D6)
message("mismatch-or-mismatch: [${s3}]")
