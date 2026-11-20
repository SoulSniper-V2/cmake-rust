set(path [=[//a//b]=])
cmake_path(GET path ROOT_NAME out)
message("[${out}]")
