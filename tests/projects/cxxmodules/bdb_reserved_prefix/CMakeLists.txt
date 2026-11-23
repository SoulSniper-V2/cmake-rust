cmake_minimum_required(VERSION 3.28)
project(CxxModulesBuildDatabaseReserved NONE)
set(CMAKE_EXPERIMENTAL_EXPORT_BUILD_DATABASE "73194a1d-c0b5-41b9-9190-a4512925e192")
set(CMAKE_EXPORT_BUILD_DATABASE ON)
add_library(iface INTERFACE)
add_library(cmake_build_database-Debug ALIAS iface)
add_library(cmake_build_database-imported INTERFACE IMPORTED)
add_library(cmake_build_database-global INTERFACE IMPORTED GLOBAL)
# One match only: the target search index is an unordered map.
