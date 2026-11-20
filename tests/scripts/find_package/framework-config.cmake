cmake_minimum_required(VERSION 4.1)
set(CMAKE_FIND_FRAMEWORK ONLY)
find_package(Acme CONFIG PATHS data/prefix NO_DEFAULT_PATH)
message("${Acme_DIR} | ${Acme_FOUND}")
