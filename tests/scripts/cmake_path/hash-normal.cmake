set(path [=[a/b/../../some/longer-than-eight/]=])
cmake_path(HASH path out)
message("[${out}]")
