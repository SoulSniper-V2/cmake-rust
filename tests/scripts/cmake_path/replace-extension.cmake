set(path [=[a//.f.txt.zip]=])
cmake_path(REPLACE_EXTENSION path json OUTPUT_VARIABLE out)
message("[${out}]")
