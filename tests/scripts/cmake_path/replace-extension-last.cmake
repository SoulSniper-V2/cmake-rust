set(path [=[a//.f.txt.zip]=])
cmake_path(REPLACE_EXTENSION path LAST_ONLY .xz OUTPUT_VARIABLE out)
message("[${out}]")
