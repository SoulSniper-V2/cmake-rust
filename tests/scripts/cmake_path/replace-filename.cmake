set(path [=[/a//b]=])
cmake_path(REPLACE_FILENAME path new/file OUTPUT_VARIABLE out)
message("[${out}]")
