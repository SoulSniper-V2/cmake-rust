set(path [=[//a]=])
cmake_path(GET path PARENT_PATH out)
message("[${out}]")
