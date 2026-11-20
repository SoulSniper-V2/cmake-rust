set(path [=[///a//b/]=])
cmake_path(GET path PARENT_PATH out)
message("[${out}]")
