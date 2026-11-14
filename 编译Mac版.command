#!/bin/bash
# Double-click in Finder: builds VideoTag.app and opens it.
cd "$(dirname "$0")"
bash packaging/macos/build-app.sh 2>&1 | tee build.log
echo ""
echo "构建结束，正在打开 VideoTag.app ……"
open VideoTag.app || true
