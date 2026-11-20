set(path [=[a//.f.txt.zip]=])
cmake_path(REMOVE_EXTENSION path OUTPUT_VARIABLE out)
message("[${out}]")
