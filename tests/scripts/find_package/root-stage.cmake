cmake_minimum_required(VERSION 4.1)
set(CMAKE_STAGING_PREFIX "${CMAKE_CURRENT_SOURCE_DIR}/data/stage")
    set(CMAKE_SYSROOT "${CMAKE_CURRENT_SOURCE_DIR}/data/root")
    find_package(Acme CONFIG PATHS data/stage NO_DEFAULT_PATH ONLY_CMAKE_FIND_ROOT_PATH)

message("found=${Acme_FOUND}|dir=${Acme_DIR}|config=${Acme_CONFIG}|considered=${Acme_CONSIDERED_CONFIGS}|versions=${Acme_CONSIDERED_VERSIONS}|origin=${origin}")
