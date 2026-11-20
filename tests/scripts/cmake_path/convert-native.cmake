cmake_path(CONVERT "a;;b/./c;;" TO_NATIVE_PATH_LIST out NORMALIZE)
message("[${out}]")
