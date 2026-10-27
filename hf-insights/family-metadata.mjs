// Public, product-level family metadata. This is not inferred from a model's uploader
// or HF `region:*` tag; source URLs point to the owning project's public site.
export const FAMILY_METADATA = {
  deepseek: { country: '中国', vendor: 'DeepSeek', official_namespaces: ['deepseek-ai'], source: 'https://www.deepseek.com/', confidence: '高' },
  qwen: { country: '中国', vendor: '阿里巴巴 / Qwen', official_namespaces: ['Qwen'], source: 'https://qwenlm.github.io/', confidence: '高' },
  glm: { country: '中国', vendor: '智谱 / Z.ai', official_namespaces: ['zai-org','THUDM'], source: 'https://www.zhipuai.cn/', confidence: '高' },
  llama: { country: '美国', vendor: 'Meta', official_namespaces: ['meta-llama'], source: 'https://ai.meta.com/llama/', confidence: '高' },
  mistral: { country: '法国', vendor: 'Mistral AI', official_namespaces: ['mistralai'], source: 'https://mistral.ai/', confidence: '高' },
  gemma: { country: '美国', vendor: 'Google', official_namespaces: ['google'], source: 'https://ai.google.dev/gemma', confidence: '高' },
  phi: { country: '美国', vendor: 'Microsoft', official_namespaces: ['microsoft'], source: 'https://azure.microsoft.com/products/phi', confidence: '高' },
  yi: { country: '中国', vendor: '01.AI', source: 'https://www.01.ai/', confidence: '中' },
  internlm: { country: '中国', vendor: '上海人工智能实验室 / InternLM', source: 'https://internlm.intern-ai.org.cn/', confidence: '中' },
  baichuan: { country: '中国', vendor: '百川智能', source: 'https://www.baichuan-ai.com/', confidence: '中' },
  vision: { country: '多来源', vendor: '视觉生成家族集合', source: null, confidence: '不适用' },
  whisper: { country: '美国', vendor: 'OpenAI', source: 'https://openai.com/research/whisper', confidence: '高' },
  encoders: { country: '多来源', vendor: '语言理解家族集合', source: null, confidence: '不适用' },
  other: { country: '未知', vendor: '未归类模型集合', source: null, confidence: '未知' },
};
