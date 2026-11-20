set(path [=[a/..]=])
cmake_path(GET path EXTENSION LAST_ONLY out)
message("[${out}]")
