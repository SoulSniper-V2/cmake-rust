set(path [=[/a/b/c/d/]=])
cmake_path(RELATIVE_PATH path BASE_DIRECTORY /a/b/x OUTPUT_VARIABLE out)
message("[${out}]")
