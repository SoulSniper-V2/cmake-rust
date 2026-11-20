set(path [=[///a//b/]=])
cmake_path(GET path RELATIVE_PART out)
message("[${out}]")
