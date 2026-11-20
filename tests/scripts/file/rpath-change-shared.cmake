file(COPY_FILE data/shared prog)
file(RPATH_CHANGE FILE prog OLD_RPATH "/same/lib" NEW_RPATH "/s2")
file(READ_ELF prog RPATH rp RUNPATH ru)
message(STATUS "rp=[${rp}] ru=[${ru}]")
