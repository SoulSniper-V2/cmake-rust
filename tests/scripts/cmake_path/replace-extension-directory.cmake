set(path [=[a//]=])
cmake_path(REPLACE_EXTENSION path zip OUTPUT_VARIABLE out)
message("[${out}]")
