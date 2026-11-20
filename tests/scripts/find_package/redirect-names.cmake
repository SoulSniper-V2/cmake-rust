cmake_minimum_required(VERSION 4.1)
set(CMAKE_FIND_PACKAGE_REDIRECTS_DIR "${CMAKE_CURRENT_SOURCE_DIR}/data/redirect")
    set(Acme_DIR "data/direct")
    find_package(Acme CONFIG NAMES Acme Alias NO_DEFAULT_PATH)

message("found=${Acme_FOUND}|dir=${Acme_DIR}|config=${Acme_CONFIG}|considered=${Acme_CONSIDERED_CONFIGS}|versions=${Acme_CONSIDERED_VERSIONS}|origin=${origin}")
