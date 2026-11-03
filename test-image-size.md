# 图片尺寸自测

准备一张图（例如本目录 `test-zh.md` 里用的任意 png），重命名或改下面的路径。
原图建议用一张 1200×600 的图，方便看出差异。

## 1) 不指定 —— 走全局上限（默认 900px）
![](big.png)

## 2) Markdown：固定 300px 宽
![默认](big.png){width=300}

## 3) Markdown：固定 800px 宽
![宽](big.png){width=800}

## 4) Markdown：占内容宽度 50%
![半宽](big.png){width=50%}

## 5) HTML：width=1000
<img src="big.png" width="1000">

## 6) HTML：百分比 30%
<img src="big.png" width="30%">

## 7) HTML：style 写法 250×125
<img src="big.png" style="width:250px; height:125px">

## 8) 只给高度 100
![高](big.png){height=100}

## 9) 超过内容区宽度（应自动收敛，不出现横向滚动）
![](big.png){width=5000}

## 10) 非法尺寸（应回落为普通图片）
![](big.png){width=auto}

---

代码块里的写法不生效，仅作示例：

```markdown
![](pic.png){width=640}
```
