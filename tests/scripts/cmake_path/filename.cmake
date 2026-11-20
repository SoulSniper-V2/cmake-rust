set(path [=[a//b/.abc]=])
cmake_path(GET path FILENAME out)
message("[${out}]")
