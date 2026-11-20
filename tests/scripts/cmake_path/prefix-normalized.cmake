set(path [=[/a/b/..]=])
cmake_path(IS_PREFIX path /a/c NORMALIZE out)
message("[${out}]")
