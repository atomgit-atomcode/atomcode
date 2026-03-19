#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
CSDN文章爬虫
功能：爬取CSDN文章标题、内容、作者、发布时间等信息
"""

import requests
from bs4 import BeautifulSoup
import time
import random
import json
import re
from datetime import datetime


class CSDNSpider:
    def __init__(self):
        self.headers = {
            'User-Agent': 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36',
            'Accept': 'text/html,application/xhtml+xml,application/xml;q=0.9,image/webp,*/*;q=0.8',
            'Accept-Language': 'zh-CN,zh;q=0.9,en;q=0.8',
            'Accept-Encoding': 'gzip, deflate, br',
            'Connection': 'keep-alive',
            'Referer': 'https://www.csdn.net/',
        }
        self.session = requests.Session()
        self.session.headers.update(self.headers)
    
    def get_article_info(self, url):
        """
        获取文章详细信息
        :param url: CSDN文章链接
        :return: 文章信息字典
        """
        try:
            # 随机延时，避免请求过于频繁
            time.sleep(random.uniform(1, 3))
            
            response = self.session.get(url, timeout=10)
            response.raise_for_status()
            response.encoding = 'utf-8'
            
            soup = BeautifulSoup(response.text, 'html.parser')
            
            # 提取文章信息
            article_info = {}
            
            # 文章标题
            title_tag = soup.find('h1', class_='title-article')
            article_info['title'] = title_tag.get_text(strip=True) if title_tag else '未知标题'
            
            # 文章ID
            article_id_match = re.search(r'article/details/(\d+)', url)
            article_info['article_id'] = article_id_match.group(1) if article_id_match else None
            
            # 作者信息
            author_tag = soup.find('a', class_='follow-nickName')
            article_info['author'] = author_tag.get_text(strip=True) if author_tag else '未知作者'
            
            # 作者主页
            author_link = author_tag.get('href') if author_tag else None
            article_info['author_homepage'] = author_link
            
            # 发布时间
            time_tag = soup.find('span', class_='time')
            article_info['publish_time'] = time_tag.get_text(strip=True) if time_tag else '未知时间'
            
            # 阅读数
            read_tag = soup.find('span', class_='read-count')
            read_text = read_tag.get_text(strip=True) if read_tag else '0'
            read_num = re.search(r'\d+', read_text)
            article_info['read_count'] = int(read_num.group()) if read_num else 0
            
            # 点赞数
            digg_tag = soup.find('span', class_='diggit')
            if digg_tag:
                digg_num = digg_tag.find('em')
                article_info['like_count'] = int(digg_num.get_text(strip=True)) if digg_num else 0
            else:
                article_info['like_count'] = 0
            
            # 评论数
            comment_tag = soup.find('span', class_='comment-count')
            comment_text = comment_tag.get_text(strip=True) if comment_tag else '0'
            comment_num = re.search(r'\d+', comment_text)
            article_info['comment_count'] = int(comment_num.group()) if comment_num else 0
            
            # 文章分类
            category_tag = soup.find('a', class_='tag-link')
            article_info['category'] = category_tag.get_text(strip=True) if category_tag else '未分类'
            
            # 文章内容
            content_tag = soup.find('article')
            article_info['content'] = content_tag.get_text(separator='\n', strip=True) if content_tag else ''
            
            # 文章HTML内容（保留格式）
            article_info['content_html'] = str(content_tag) if content_tag else ''
            
            # 文章链接
            article_info['url'] = url
            
            # 爬取时间
            article_info['crawl_time'] = datetime.now().strftime('%Y-%m-%d %H:%M:%S')
            
            return article_info
            
        except requests.RequestException as e:
            print(f"请求错误: {e}")
            return None
        except Exception as e:
            print(f"解析错误: {e}")
            return None
    
    def save_to_json(self, article_info, filename=None):
        """
        保存文章信息到JSON文件
        :param article_info: 文章信息字典
        :param filename: 保存的文件名，默认使用文章ID
        """
        if not article_info:
            print("文章信息为空，无法保存")
            return
        
        if not filename:
            article_id = article_info.get('article_id', 'unknown')
            filename = f"csdn_article_{article_id}.json"
        
        try:
            with open(filename, 'w', encoding='utf-8') as f:
                json.dump(article_info, f, ensure_ascii=False, indent=2)
            print(f"文章已保存到: {filename}")
        except Exception as e:
            print(f"保存文件错误: {e}")
    
    def save_to_txt(self, article_info, filename=None):
        """
        保存文章内容到TXT文件
        :param article_info: 文章信息字典
        :param filename: 保存的文件名，默认使用文章标题
        """
        if not article_info:
            print("文章信息为空，无法保存")
            return
        
        if not filename:
            title = article_info.get('title', 'unknown')
            # 清理文件名中的非法字符
            title = re.sub(r'[<>:"/\\|?*]', '', title)
            filename = f"{title}.txt"
        
        try:
            with open(filename, 'w', encoding='utf-8') as f:
                f.write(f"标题: {article_info.get('title', '')}\n")
                f.write(f"作者: {article_info.get('author', '')}\n")
                f.write(f"发布时间: {article_info.get('publish_time', '')}\n")
                f.write(f"阅读数: {article_info.get('read_count', 0)}\n")
                f.write(f"点赞数: {article_info.get('like_count', 0)}\n")
                f.write(f"评论数: {article_info.get('comment_count', 0)}\n")
                f.write(f"分类: {article_info.get('category', '')}\n")
                f.write(f"链接: {article_info.get('url', '')}\n")
                f.write(f"爬取时间: {article_info.get('crawl_time', '')}\n")
                f.write("\n" + "="*50 + "\n\n")
                f.write(article_info.get('content', ''))
            print(f"文章已保存到: {filename}")
        except Exception as e:
            print(f"保存文件错误: {e}")
    
    def print_article_info(self, article_info):
        """
        打印文章信息
        :param article_info: 文章信息字典
        """
        if not article_info:
            print("文章信息为空")
            return
        
        print("\n" + "="*60)
        print(f"标题: {article_info.get('title')}")
        print(f"作者: {article_info.get('author')}")
        print(f"发布时间: {article_info.get('publish_time')}")
        print(f"阅读数: {article_info.get('read_count')}")
        print(f"点赞数: {article_info.get('like_count')}")
        print(f"评论数: {article_info.get('comment_count')}")
        print(f"分类: {article_info.get('category')}")
        print(f"链接: {article_info.get('url')}")
        print("="*60)
        print(f"内容预览:\n{article_info.get('content', '')[:200]}...")
        print("="*60 + "\n")


def main():
    """主函数"""
    spider = CSDNSpider()
    
    # 从用户输入获取文章链接
    print("CSDN文章爬虫")
    print("="*60)
    url = input("请输入CSDN文章链接: ").strip()
    
    if not url or 'csdn.net' not in url:
        print("请输入有效的CSDN文章链接")
        return
    
    print(f"\n正在爬取: {url}")
    article_info = spider.get_article_info(url)
    
    if article_info:
        # 打印文章信息
        spider.print_article_info(article_info)
        
        # 保存为JSON
        spider.save_to_json(article_info)
        
        # 保存为TXT
        spider.save_to_txt(article_info)
        
        print("\n爬取完成！")
    else:
        print("爬取失败，请检查链接是否正确")


if __name__ == '__main__':
    main()
