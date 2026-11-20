set(path [=[//a//b]=])
cmake_path(REMOVE_FILENAME path OUTPUT_VARIABLE out)
message("[${out}]")
