cmake_path(CONVERT "a::/b//c/../:z:" TO_CMAKE_PATH_LIST out NORMALIZE)
message("[${out}]")
