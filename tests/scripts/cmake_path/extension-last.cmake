set(path [=[a/f.tar.gz]=])
cmake_path(GET path EXTENSION LAST_ONLY out)
message("[${out}]")
