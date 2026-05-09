1. memory相关代码命名以及数据库命名都改成`capability_capsule`,类似能力胶囊
2. 清理删除不相关的数据库操作以及代码
3. 梳理mcp中提供的能力
4. claude hooks中相关的数据
5. 现在`transcripts`中数据现在只存了，没有使用的地方
6. 数据库文档以及之间的依赖关系
7. 不需要加feature了，目前用lancedb + duckdb的方式，`storage`做按照`capability_capsule`和`transcripts`模块做抽象即可
8. cli中的有些共通参数合并成一个struct，如`tenant`和`base_url`
9. tiktoken-rs现在还有没有用途