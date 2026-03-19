# CSDN文章爬虫

一个简单实用的CSDN文章爬虫工具，可以爬取文章的标题、内容、作者、发布时间等信息。

## 功能特点

- 爬取CSDN文章完整内容
- 提取文章标题、作者、发布时间等元数据
- 获取阅读数、点赞数、评论数等统计数据
- 支持保存为JSON和TXT格式
- 随机延时避免被封禁
- 完善的错误处理机制

## 安装依赖

```bash
pip install -r requirements_csdn.txt
```

或者手动安装：

```bash
pip install requests beautifulsoup4 lxml
```

## 使用方法

### 方式1：交互式运行

```bash
python csdn_spider.py
```

运行后输入CSDN文章链接即可。

### 方式2：在代码中调用

```python
from csdn_spider import CSDNSpider

# 创建爬虫实例
spider = CSDNSpider()

# 爬取文章
url = "https://blog.csdn.net/xxx/article/details/xxx"
article_info = spider.get_article_info(url)

# 打印文章信息
if article_info:
    spider.print_article_info(article_info)
    
    # 保存为JSON
    spider.save_to_json(article_info)
    
    # 保存为TXT
    spider.save_to_txt(article_info)
```

### 方式3：批量爬取

```python
from csdn_spider import CSDNSpider

spider = CSDNSpider()

# 文章链接列表
urls = [
    "https://blog.csdn.net/xxx/article/details/111",
    "https://blog.csdn.net/xxx/article/details/222",
    "https://blog.csdn.net/xxx/article/details/333",
]

for url in urls:
    article_info = spider.get_article_info(url)
    if article_info:
        spider.save_to_json(article_info)
        spider.save_to_txt(article_info)
```

## 输出格式

### JSON格式

```json
{
  "title": "文章标题",
  "article_id": "123456789",
  "author": "作者名",
  "author_homepage": "作者主页链接",
  "publish_time": "2023-01-01 12:00:00",
  "read_count": 1000,
  "like_count": 50,
  "comment_count": 10,
  "category": "分类",
  "content": "文章内容...",
  "content_html": "文章HTML内容...",
  "url": "文章链接",
  "crawl_time": "爬取时间"
}
```

### TXT格式

```
标题: 文章标题
作者: 作者名
发布时间: 2023-01-01 12:00:00
阅读数: 1000
点赞数: 50
评论数: 10
分类: 分类
链接: 文章链接
爬取时间: 爬取时间

==================================================

文章内容...
```

## 注意事项

1. **遵守robots.txt**：请遵守CSDN的robots协议，不要过度爬取
2. **添加延时**：代码已内置随机延时（1-3秒），避免请求过于频繁
3. **仅用于学习**：本工具仅供学习交流使用，请勿用于商业用途
4. **内容版权**：爬取的文章内容版权归原作者所有

## 高级功能

### 设置代理

```python
spider = CSDNSpider()

# 设置代理
proxies = {
    'http': 'http://127.0.0.1:7890',
    'https': 'http://127.0.0.1:7890',
}
spider.session.proxies.update(proxies)
```

### 添加Cookie

```python
spider = CSDNSpider()

# 添加Cookie（如果需要登录态）
spider.session.cookies.set('cookie_name', 'cookie_value')
```

## 常见问题

**Q: 爬取失败怎么办？**

A: 检查以下几点：
- 网络连接是否正常
- 文章链接是否有效
- 是否被CSDN反爬虫拦截（尝试添加更多延时或使用代理）

**Q: 为什么有些内容爬取不到？**

A: CSDN页面结构可能会更新，某些元素的选择器可能需要调整。

**Q: 可以爬取私密文章吗？**

A: 私密文章需要登录态，需要添加Cookie才能访问。

## License

MIT License
