# Change Log

All notable changes to the "vscode-git-ai" extension will be documented in this file.

Check [Keep a Changelog](http://keepachangelog.com/) for recommendations on how to structure this file.

## [Unreleased]

## [0.1.24]

- 同步 Git AI 1.6.17 的 Hook/Checkpoint 可靠性修复：严格模式下后台拒绝、超时和缺失上下文不再被静默当作成功。
- Checkpoint 待处理工作改为可恢复队列，失败会保留并重试；非 Git 文件编辑不再误落到无关工作区。

## [0.1.23]

- 新增 Git AI Activity Bar 的“数据上报”页面：从 Kilo 的本地设置补齐 Git AI 空字段，并允许用户维护上报服务器、组织与人员信息。
- 组织下拉选项由所填上报服务器加载，支持缓存、超时和失效值提示。
- 企业上报身份仅用于 Git AI 指标，不写入 Git Notes。
