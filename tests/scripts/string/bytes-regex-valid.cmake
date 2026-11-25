string(REGEX MATCHALL "é" matches "aébé")
file(WRITE valid-regex.bin "${matches}")
file(READ valid-regex.bin hex HEX)
message("${hex}")
