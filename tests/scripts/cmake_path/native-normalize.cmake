set(path [=[a//b/../c]=])
cmake_path(NATIVE_PATH path NORMALIZE out)
message("[${out}]")
