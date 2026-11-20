cmake_minimum_required(VERSION 4.1)
set(CMAKE_IGNORE_PREFIX_PATH "/pkg")
    set(CMAKE_FIND_ROOT_PATH "${CMAKE_CURRENT_SOURCE_DIR}/data/root")
    find_package(Acme CONFIG PATHS /pkg /fallback NO_DEFAULT_PATH ONLY_CMAKE_FIND_ROOT_PATH)

message("found=${Acme_FOUND}|dir=${Acme_DIR}|config=${Acme_CONFIG}|considered=${Acme_CONSIDERED_CONFIGS}|versions=${Acme_CONSIDERED_VERSIONS}|origin=${origin}")
