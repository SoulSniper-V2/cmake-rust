set(path [=[.profile.tar.gz]=])
cmake_path(GET path STEM LAST_ONLY out)
message("[${out}]")
