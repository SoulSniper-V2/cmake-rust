cmake_minimum_required(VERSION 4.1)
set(CMAKE_FIND_PACKAGE_PREFER_CONFIG OFF)
        set(CMAKE_MODULE_PATH "data/modules")
        set(CMAKE_PREFIX_PATH "data/prefix")
        find_package(Acme)

message("found=${Acme_FOUND}|dir=${Acme_DIR}|config=${Acme_CONFIG}|considered=${Acme_CONSIDERED_CONFIGS}|versions=${Acme_CONSIDERED_VERSIONS}|origin=${origin}")
