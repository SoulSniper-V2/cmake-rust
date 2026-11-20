set(path [=[a/f.tar.gz]=])
cmake_path(GET path EXTENSION out)
message("[${out}]")
