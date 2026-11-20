set(vals "a;b")
cmake_language(CALL message "${vals}")
cmake_language(CALL set joined "${vals}")
message("joined=${joined}")
