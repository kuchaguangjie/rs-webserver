# ============================================================
# rs-webserver Makefile
# ------------------------------------------------------------
# 常用命令：
#   make            # 等同于 make build
#   make run        # 使用 config.yml 启动服务器
#   make smoke      # 启动后对几个路由做一次冒烟测试（需已安装 curl）
#   make check      # 类型检查
#   make test       # 运行单元测试
#   make fmt/clippy # 代码格式化 / 静态检查
#   make clean      # 清理编译产物
# ============================================================

# 可通过环境变量覆盖，例如：make run CONFIG=my.yml
CARGO  ?= cargo
CONFIG ?= config.yml
HOST   ?= 127.0.0.1
PORT   ?= 7878

# 让 `make` 默认构建。
.DEFAULT_GOAL := build

.PHONY: all build run release check test fmt clippy clean smoke help

all: build

## 构建（debug）
build:
	$(CARGO) build

## 运行服务器（默认读取 config.yml，可用 CONFIG=... 覆盖）
run:
	$(CARGO) run -- $(CONFIG)

## 构建（release，开启优化）
release:
	$(CARGO) build --release

## 类型检查
check:
	$(CARGO) check --all-targets

## 运行单元测试
test:
	$(CARGO) test

## 格式化代码
fmt:
	$(CARGO) fmt

## 静态检查（把警告视为错误）
clippy:
	$(CARGO) clippy --all-targets -- -D warnings

## 清理编译产物
clean:
	$(CARGO) clean

## 冒烟测试：后台启动服务器，请求各路由后关闭
smoke: build
	@echo ">> 启动服务器用于冒烟测试..."
	@$(CARGO) run -- $(CONFIG) & \
	pid=$$!; \
	sleep 1; \
	echo ">> GET /";        curl -sS -o /dev/null -w '  %{http_code}\n' http://$(HOST):$(PORT)/; \
	echo ">> GET /nope";    curl -sS -o /dev/null -w '  %{http_code}\n' http://$(HOST):$(PORT)/nope; \
	echo ">> GET /sleep";   curl -sS -o /dev/null -w '  %{http_code}\n' http://$(HOST):$(PORT)/sleep; \
	kill $$pid 2>/dev/null || true; \
	wait $$pid 2>/dev/null || true

## 显示本帮助
help:
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/## //'
