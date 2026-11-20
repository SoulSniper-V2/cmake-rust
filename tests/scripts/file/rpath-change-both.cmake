file(COPY_FILE data/both prog)
file(RPATH_CHANGE FILE prog OLD_RPATH "/r/lib" NEW_RPATH "/new")
file(READ_ELF prog RPATH rp RUNPATH ru)
message(STATUS "rp=[${rp}] ru=[${ru}]")
