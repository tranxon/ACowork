# AI Agent 记忆系统评估框架调研报告

**调研日期**：2026-04-21
**调研范围**：LongMemEval、LoCoMo-Plus、BEAM
**用途**：RollBall.AI 记忆系统质量评估体系参考

---

## 研究报告：AI Agent 记忆系统评估框架分析

### 调查目标
分析三个主要 AI Agent 记忆系统评估框架（LongMemEval、LoCoMo-Plus、BEAM），评估其对 RollBall.AI 三层五类记忆架构（瞬态层/经历层/沉淀层，Grafeo 图数据库存储）的适用性，并提出分阶段实现建议。

---

## 第一部分：三个框架核心对比

### 1.1 LongMemEval（ICLR 2025）

**核心特征：** 针对长期交互记忆的综合基准  
**发布方：** 微软/CMU（论文作者 Di Wu 等）  
**开源状态：** ✅ 完全开源  
- GitHub: `xiaowu0162/LongMemEval`
- HuggingFace 数据集已发布
- Python 实现，支持自定义评估脚本

**评估维度（5 个核心能力）：**

| 维度 | 定义 | 评估内容 | 数据特征 |
|------|------|---------|---------|
| Information Extraction (IE) | 从对话中精确提取显式信息 | 单一事实提取（如"用户的职位是什么"） | 单会话问题 |
| Multi-Session Reasoning (MR) | 跨多个会话整合信息进行推理 | 需要组合多个会话中的片段信息 | 典型 2-3 个会话的关联 |
| Temporal Reasoning (TR) | 处理时间相关的推理和因果关系 | 事件顺序、时间差计算、时序逻辑 | 含明确时间戳 |
| Knowledge Updates (KU) | 处理信息更新和冲突解决 | 同一事实的多个版本，需识别最新版本 | 故意包含信息修订 |
| Abstention (Abs) | 知道什么时候不知道 | 区分"找不到答案"vs"错误回答" | 设计无答案的问题 |

**数据集规模：**
- 总计 500 个精心标注的问题
- 涵盖 3 个难度等级：LongMemEval-S（~115K tokens，~40 sessions）、LongMemEval-M（~500 sessions）、LongMemEval-Oracle（仅证据会话）
- 问题类型按上述 5 个维度分类
- 包含 ~40 个会话的对话历史（平均 9K tokens/对话）

**指标定义与计算方式：**

```
基础指标：
├─ Accuracy (精确匹配率)
│  └─ 规则：模型输出 == 标准答案（完全字符串匹配）
├─ F1 Score（Flex 评估）
│  └─ 用于部分匹配的答案（如实体列表）
├─ Turn-Level Recall（转折级召回率）
│  └─ 规则：模型是否找到包含答案的正确对话转折
└─ Session-Level Recall（会话级召回率）
   └─ 规则：模型是否检索到包含证据的会话 ID

自动评估方法：
├─ 路径 1：字符串匹配（IE/Abs 类问题）
├─ 路径 2：LLM-as-Judge（GPT-4o）
│  └─ Prompt：比较模型输出与标准答案的语义一致性
│  └─ 输出：Binary（0/1）或 Likert scale（1-5）
└─ 路径 3：Mixed（优先字符串，难以判断时调用 LLM）
```

**自动评估流程（核心创新）：**
```python
# 伪代码自 evaluate_qa.py
for question in questions:
    if question_type in ["single_session", "knowledge_update"]:
        # 直接字符串匹配
        score = exact_match(model_output, gold_answer)
    else:
        # 调用 GPT-4o 作为裁判
        score = llm_judge(
            question=question,
            gold_answer=gold_answer,
            model_output=model_output,
            judge_model="gpt-4o"
        )
    metrics[question_type].append(score)
```

**数据集构成特点：**
- **属性受控生成管道**：通过设置用户背景、时间轴、事件序列，系统性地生成演变的对话历史
- **时间戳注入**：每个会话带有明确的日期，支持时间推理评估
- **可扩展的"干草堆"设计**：可任意增加无关会话数量，测试检索在噪声中的表现
- **证据标注**：每个答案标注来源会话和具体转折，支持会话级/转折级的召回评估

**开源实现特点：**
- Python 3.9+，依赖轻量（torch, transformers, openai）
- 支持本地离线评估（仅需 Python 环境）
- 支持自定义 LLM 后端（默认 GPT-4o，可配置其他模型）
- 提供数据自定义编译脚本，支持任意长度的对话扩展

---

### 1.2 LoCoMo-Plus（2026年2月提交，扩展原 LoCoMo）

**核心特征：** 超越因式记忆的认知记忆评估框架  
**发布方：** 清华大学/Snap Research（多机构合作）  
**开源状态：** ✅ 部分开源  
- GitHub: `xjtuleeyf/Locomo-Plus`
- 评估框架和数据集可用，但部分预处理代码未开源

**评估维度（扩展原 LoCoMo + 第 6 个维度）：**

原 LoCoMo（5 个任务类型）：
| 任务 | 描述 | 难度 |
|------|------|------|
| QA (Question Answering) | 事实回忆和推理 | 基础 |
| Event Summarization | 生成事件摘要 | 中等 |
| Multimodal Dialogue Generation | 多模态对话生成 | 高等 |
| (3 个隐式任务) | - | - |

**LoCoMo-Plus 新增第 6 个维度：**
- **Cognitive Memory（认知记忆）**：超越表面事实回忆，评估隐式约束的保留与应用
- **核心创新**：提出"Cue-Trigger 语义不匹配"设置
  - 问题线索与触发记忆的信息在表面上无直接关联
  - 需要模型理解隐式的用户状态、目标、价值观
  - 示例：用户未明确说"我讨厌加班"，但通过 3 次拒绝加班任务推断

**数据集规模：**
- 继承 LoCoMo 的 ~300 turns/对话、~9K tokens/对话、~35 sessions
- 新增认知记忆特定数据：设计 cue-trigger 不匹配的对话对
- 包含人工和 LLM 双注（Human Judge + LLM Judge）

**评估方法的关键差异：**

```
LoCoMo（传统方法）：
├─ 字符串匹配
├─ ROUGE/BLEU（生成任务）
└─ 常规准确率

LoCoMo-Plus（新框架）：
├─ Constraint Consistency Score（约束一致性评估）
│  └─ 定义：模型回应是否遵守隐含的用户约束
│  └─ 计算：LLM-as-Judge 比对回应与推断的约束
├─ Beyond String-Matching Metrics
│  └─ 问题：传统字符串匹配对认知记忆不适用
│  └─ 解决：Semantic Consistency Evaluation（语义一致性）
└─ 人工标注补充
   └─ 规则：50% 的数据用 Human 标注验证
```

**核心评估指标定义：**
```
约束一致性评分 (Constraint Consistency Score, CCS):
CCS = |{回应中遵守的约束} ∩ {推断的用户约束}| 
      / |{推断的用户约束}|

范围：0.0 - 1.0
解释：
  - 1.0 = 完全遵守所有推断约束
  - 0.5 = 部分遵守
  - 0.0 = 完全忽视或违反约束

评估流程（LLM-as-Judge 关键步骤）：
1. 提取对话中的隐式约束（使用 LLM）
2. 生成模型回应
3. 检查回应是否违反约束（使用另一个 LLM）
4. 计算约束遵守率

长度偏差处理：
├─ 识别问题：模型倾向于输出长文本以"隐藏"记忆错误
├─ 解决：控制生成长度，分离长度与记忆质量的评估
└─ 指标：Length-normalized CCS
```

**数据集构成特点：**
- **Cue-Trigger 语义不匹配设计**：系统性地创建线索-触发记忆间隔大的对话
- **隐式约束标注**：每个对话标注推断出的用户约束列表
- **多模态支持**：继承原 LoCoMo 的图像、视频支持
- **双评估模式**：支持 Human Judge 和 LLM Judge，并提供一致性分析

**开源实现特点：**
- 代码部分开源（评估脚本 + 数据预处理）
- 评估框架可在自有数据上应用
- 支持自定义约束定义和评估标准

---

### 1.3 BEAM（Benchmarking and Enhancing Long-Term Memory，ICLR 2026）

**核心特征：** 超大规模对话长度（最长 10M tokens）的记忆评估基准  
**发布方：** University of Alberta / UMass Amherst  
**开源状态：** ✅ 完全开源  
- GitHub: `mohammadtavakoli78/BEAM`
- HuggingFace 数据集：`Mohammadta/BEAM`
- Python 实现 + 完整训练/评估脚本

**评估维度（10 个记忆能力类型）：**

| 能力 | 定义 | 评估方式 |
|------|------|---------|
| 1. Information Retention | 保留事实信息 | 事实标注和回忆 |
| 2. Temporal Understanding | 时间序列理解 | 事件顺序判断 |
| 3. Multi-hop Reasoning | 多跳推理 | 需要 2+ 个推理步骤 |
| 4. Contradiction Detection | 矛盾检测 | 识别冲突信息 |
| 5. Entity Tracking | 实体追踪 | 跨会话实体一致性 |
| 6. Relationship Inference | 关系推断 | 从对话推断隐式关系 |
| 7. Preference Learning | 偏好学习 | 用户偏好的推断和应用 |
| 8. Contextual Relevance | 语境相关性 | 信息相关性判断 |
| 9. Knowledge Evolution | 知识演化 | 信息更新处理 |
| 10. Selective Recall | 选择性回忆 | 优先级记忆排序 |

**数据集规模（创新之处）：**
- **总体规模**：100 个对话（每个对话呈现 5 个长度版本）
- **对话长度范围**：
  - 100K tokens（典型 Llama3 16K 上下文的 6-8 倍）
  - 500K tokens
  - 1M tokens
  - 5M tokens
  - 10M tokens（超出大多数 LLM 上下文窗口）
- **总计标注问题**：2,000 个验证的探针问题
- **问题分布**：按 10 个能力均匀分布

**数据生成管道（区别于其他框架）：**

```
核心创新：Hierarchical Decomposition + Sequential Expansion

1️⃣ 用户背景生成
   ├─ 个人信息（年龄、职业、地点等）
   └─ 核心关系（固定不变）

2️⃣ 主题种子→十个子种子（层级分解）
   ├─ 每个子种子代表一个时间/话题段
   ├─ 子种子之间保持因果连贯性
   └─ 新伙伴逐步引入（反映真实关系演化）

3️⃣ 对话计划生成
   ├─ 10 个计划对应 10 个子种子
   ├─ 每个计划含 30-50 个对话轮次
   └─ 保留用户核心关系，引入新角色

4️⃣ 用户/助手话语生成
   ├─ 基于计划使用 LLM 生成现实对话
   └─ 保持人物一致性和对话自然流

5️⃣ 探针问题生成
   ├─ 从对话中提取关键信息点（bullet points）
   ├─ 用 GPT-4-mini 生成问题和标准答案
   ├─ 标注来源消息和验证
   └─ 人工验证生成的问题质量

关键特性：
- 层级分解确保了 10M token 对话的内部一致性
  （避免其他框架的"人工拼接"问题）
- 滑动窗口处理确保可扩展性
- 多层次验证（LLM + Human）
```

**评估指标与计算方式：**

```
基础指标：
├─ Accuracy @ Length：按对话长度统计准确率
│  └─ 显示性能如何随长度衰减
├─ F1 Score：对于有多个正确答案的问题
├─ Mean Reciprocal Rank (MRR)：检索排序质量
└─ Token-aware 指标
   └─ 考虑对话长度的相对难度

高阶指标：
├─ Memory Robustness Score
│  └─ 模型在极长对话中的稳定性
├─ Retrieval Efficiency
│  └─ 检索所需的 token 消耗 vs 准确率
└─ Breakdown Analysis
   └─ 在 10M tokens 下的突然失败点

性能衰减曲线分析：
- 在 500K tokens 处开始显著性能下降
- 在 10M tokens 处上下文填充方法完全失效
- 检索增强方法相对稳定
```

**自动评估工具链：**
```python
# BEAM 评估框架组件
class BEAMEvaluator:
    def __init__(self, benchmark_config):
        self.probing_questions = load_questions()
        self.gold_answers = load_answers()
    
    def evaluate_by_length(self, model, dialogue_lengths=[100k, 500k, 1m, 5m, 10m]):
        results = {}
        for length in dialogue_lengths:
            dialogue = truncate_or_pad(dialogue, length)
            predictions = model.generate(dialogue)
            results[length] = {
                'accuracy': compute_accuracy(predictions, gold_answers),
                'f1': compute_f1(predictions, gold_answers),
                'mrr': compute_mrr(predictions, gold_answers)
            }
        return results
    
    def plot_degradation_curve(self, results):
        # 显示性能随长度的衰减轨迹
        pass
```

**LIGHT 框架（BEAM 配套方法）：**

BEAM 论文不仅引入基准，还提出 LIGHT 框架来改进性能：
```
LIGHT 三层架构（模拟人类认知）：

┌─ Episodic Memory（情节记忆）─┐
│  • 从对话检索相关片段         │
│  • 使用嵌入模型 + 向量索引     │
│  • 返回 Top-K 相关段落         │
├─ Working Memory（工作记忆）──┤
│  • 最近 Z 个对话轮次          │
│  • 保持新近性上下文            │
├─ Scratchpad（便签本）────────┤
│  • 迭代积累的要点笔记         │
│  • 支持多轮补充更新           │
│  • 包含语义/自传体/目标等     │
└─ LLM 生成────────────────────┘
   综合三层记忆生成回答
```

**开源实现特点：**
- 完整的数据生成管道（可定制参数）
- 支持多种骨干模型（开源+商用）
- 完整的评估脚本和结果分析工具
- 预计算的嵌入和索引（加速复现）

---

## 第二部分：三个框架的详细对比

### 2.1 核心维度对比表

| 维度 | LongMemEval | LoCoMo-Plus | BEAM |
|------|------------|-----------|------|
| **论文发表** | ICLR 2025 | 2026.02 | ICLR 2026 |
| **开源完整度** | ✅ 100% | ⚠️ 70% | ✅ 100% |
| **主要贡献** | 五维记忆评估 | 认知记忆框架 | 超大规模长度测试 |
| **对话数量** | 500 questions | 原 LoCoMo × N | 100 conversations |
| **单对话长度** | ~9K avg, 40-500 sessions | ~9K avg, 35 sessions | 100K-10M tokens |
| **总题目数** | 500 | ~1000+ | 2,000 |
| **评估维度** | 5 (IE/MR/TR/KU/Abs) | 5+1 (加 Cognitive) | 10 (细粒度能力) |
| **数据生成方式** | 属性受控 + 人工标注 | 继承 LoCoMo 扩展 | 层级分解 + 顺序扩展 |
| **评估方法** | 混合（字符串+LLM） | 约束一致性评分 | 长度衰减曲线分析 |
| **LLM-as-Judge** | ✅ GPT-4o | ✅ 自定义 | ✅ 支持 |
| **实现语言** | Python 3.9+ | Python | Python |
| **Rust 实现** | ❌ | ❌ | ❌ |
| **数据集大小** | ~200MB | ~500MB | ~1GB+ |
| **关键指标** | Accuracy, F1, Turn/Session Recall | CCS, Constraint Coverage | Accuracy@Length, Degradation Curve |

### 2.2 关键指标对比

**LongMemEval 指标体系：**
```
Turn-Level Recall (转折级召回)
  = {转折 i 包含答案且被模型检索到} / {包含答案的转折总数}
  范围：0-1

Session-Level Recall (会话级召回)
  = {会话 j 包含答案且被模型检索到} / {包含答案的会话总数}
  范围：0-1

Question-Level Accuracy (问题级准确率)
  = {完全正确回答的问题数} / {总问题数}
  范围：0-1

F1 Score (Flex)
  = 2 × (Precision × Recall) / (Precision + Recall)
  用于多选或部分匹配场景
```

**LoCoMo-Plus 指标体系：**
```
Constraint Consistency Score (CCS)
  = Σ(满足约束的回应) / 总推断约束数
  范围：0-1
  创新点：不依赖字符串匹配

Constraint Coverage
  = {在回应中明确体现的约束} / {所有隐含约束}
  范围：0-1

Semantic Consistency (语义一致性)
  = LLM评分(回应与约束的一致性) / 5
  范围：0-1
  评价：更接近人类判断

Long-Context Performance Drop (LCPD)
  = (短对话准确率 - 长对话准确率) / 短对话准确率
  单位：百分比
  监控：对话长度的负面影响
```

**BEAM 指标体系：**
```
Accuracy@Length[L]
  = 在长度 L 的对话上正确回答的问题比例
  L ∈ {100K, 500K, 1M, 5M, 10M}

Memory Degradation Slope (MDS)
  = (Accuracy@100K - Accuracy@10M) / log10(100M)
  衡量性能下降速率
  陡峭斜率 = 差的记忆架构

Retrieval Effectiveness Ratio (RER)
  = Accuracy(with retrieval) / Accuracy(no retrieval)
  范围：1.0+
  衡量检索的帮助程度

Token Efficiency Score (TES)
  = Accuracy / (Context Tokens Used)
  衡量效率（准确率 per token）
```

---

## 第三部分：与 RollBall 三层记忆架构的映射关系

### 3.1 架构层级映射

**RollBall 三层记忆架构：**
```
瞬态层（Transient）- 工作记忆
  ├─ LLM 上下文窗口
  ├─ 当前对话推理链
  └─ System Prompt + Retrieved Memories

经历层（Experiential）- 情景记忆
  ├─ Episodes 表（Grafeo）
  ├─ HNSW 向量索引 + BM25 全文检索
  ├─ 生命周期：天到周，巩固后晋升沉淀层
  └─ 向量 + 全文混合检索 (RRF)

沉淀层（Consolidated）- 长期记忆
  ├─ KnowledgeNode（语义：事实/偏好/关系）
  ├─ ProceduralNode（程序：行为模式）
  ├─ AutobiographicalNode（自传体：自我认知）
  ├─ LPG 知识图谱 + memory_edges
  └─ 关联扩散检索（1-2 跳）
```

**三个框架对应层级：**

| RollBall 层 | LongMemEval 对应 | LoCoMo-Plus 对应 | BEAM 对应 |
|-----------|--------------|--------------|--------|
| 瞬态层 | System Prompt + Context Injection | Working Memory | Working Memory (LIGHT) |
| 经历层 | Chat History (Sessions) | Dialog Turns (35 sessions) | Episodic Memory (LIGHT) |
| 沉淀层 | 隐含（通过多会话推理） | 隐式约束提取 | Scratchpad (LIGHT) |

**映射细节：**

```
1. Information Extraction (LongMemEval IE)
   → RollBall 经历层 episode 的精确检索
   → 对应：hybrid_search 的字符串精确匹配
   
2. Multi-Session Reasoning (LongMemEval MR)
   → RollBall 沉淀层的 KnowledgeNode 关联
   → 对应：graph_expand 跨节点推理
   
3. Temporal Reasoning (LongMemEval TR)
   → RollBall episodic 的时间戳过滤
   → 对应：MemoryQuery 中的 time_range 参数
   
4. Knowledge Updates (LongMemEval KU)
   → RollBall 沉淀层的冲突解决机制
   → 对应：decay_score 和 confidence 更新

5. Abstention (LongMemEval Abs)
   → RollBall 检索失败时的 fallback
   → 对应：min_score 阈值和"无匹配"返回

6. Cognitive Memory (LoCoMo-Plus)
   → RollBall ProceduralNode + AutobiographicalNode
   → 对应：隐式约束的 System Prompt 注入

7. Memory Robustness (BEAM MDS)
   → RollBall decay_score 和 Dormant 状态
   → 对应：长期不访问的记忆衰减曲线
```

---

### 3.2 直接适用维度（无需改造）

这些维度可直接从框架评估指标复用到 RollBall：

**1. Information Extraction Accuracy（信息提取准确率）**
- ✅ 直接适用
- 方式：评估 hybrid_search 的精确召回
- RollBall 测试：向 episode 中精确问题，验证返回正确转折
- 指标：Turn-Level Recall（转折级别准确率）
- 计算：`被正确检索的包含答案的转折 / 所有包含答案的转折`

**2. Multi-Session Reasoning（多会话推理）**
- ✅ 直接适用
- 方式：评估 graph_expand 的多跳推理
- RollBall 测试：需要组合多个 episode 和 KnowledgeNode 的问题
- 指标：Multi-Session Accuracy
- 计算：需要正确关联 >=2 个认知层的问题比例

**3. Temporal Reasoning（时间推理）**
- ✅ 直接适用（RollBall 已支持 timestamp）
- 方式：评估含时间戳的 episode 过滤
- RollBall 测试：事件顺序问题、时间间隔计算
- 指标：Temporal Accuracy
- 计算：`正确理解时间关系的问题 / 时间类问题总数`

**4. Knowledge Update Handling（知识更新处理）**
- ✅ 直接适用
- 方式：评估冲突节点的解决
- RollBall 测试：同一事实多个版本，模型应返回最新
- 指标：Recency-Aware Accuracy
- 计算：`识别最新信息的问题 / 有知识更新的问题总数`

**5. Constraint Consistency Score（LoCoMo-Plus）**
- ⚠️ 部分适用
- 方式：评估 ProceduralNode 的隐式约束遵守
- RollBall 改造：
  - 提取 ProceduralNode 中的 trigger_condition 作为约束
  - 验证模型回应是否遵守这些约束
- 例子：
  ```
  隐含约束：用户偏好简洁回复 (ProceduralNode)
  测试：Agent 在类似场景下是否生成了简洁回复
  评估：遵守约束的比例
  ```

---

### 3.3 需要改造/补充的维度

**1. Abstention（知道何时不知道）**
- ⚠️ 需要补充
- RollBall 当前状态：无专门机制区分"找不到"vs"错误回答"
- 改造方案：
  ```
  在 MemoryQuery 中新增 confidence_threshold 参数
  ├─ 检索分数 < threshold → 返回 "不确定" 信号
  ├─ Agent System Prompt 包含：识别 "不确定" 信号时说 "我不确定"
  └─ 评估指标：Abstention Accuracy = 正确拒绝回答的问题 / 无答案问题
  ```

**2. Long-Context Robustness（超长上下文鲁棒性）**
- ⚠️ 需要新增测试
- RollBall 当前状态：未系统测试在极长对话（1M+ tokens）下的表现
- 改造方案：
  ```
  Phase 3 实现前置测试：
  ├─ 创建 100K/500K/1M tokens 的标准测试集
  ├─ 评估每个长度下的 Accuracy 衰减曲线
  ├─ 监控衰减速率（MDS）
  └─ 验证 decay_score 在极长对话下的有效性
  ```

**3. Artifact-Aware Retrieval（工件感知检索）**
- ❌ 三个框架都未覆盖
- RollBall 特有：episode 中含 artifact_refs，需要评估模型是否正确利用
- 新增指标：
  ```
  Artifact Recall Accuracy
  = {正确识别并返回相关 artifact 的问题} 
    / {涉及 artifact 的问题}
  
  Example：
  - Episode 包含：代码修改（artifact_refs 指向 src/main.rs）
  - 问题：这次改动影响了什么？
  - 评估：模型是否找到了 artifact_refs 并理解其含义
  ```

**4. Episodic-Semantic Bridge（经历-沉淀跨层检索）**
- ⚠️ LoCoMo-Plus 部分涉及
- RollBall 特有需求：评估 source_episode 反向查询的有效性
- 新增指标：
  ```
  Cross-Layer Retrieval Effectiveness
  = {通过 source_episode 反向查询到相关节点的问题} 
    / {需要跨层检索的问题}
  
  二阶指标：Edge Weight Quality
  = 实际使用的边权重分布 vs 标准分布
  ```

---

## 第四部分：开源实现状态与语言生态分析

### 4.1 现有开源实现总结

| 框架 | 开源完整度 | Python | Rust | 其他 | 关键文件 |
|------|----------|-------|------|------|---------|
| **LongMemEval** | ✅ 100% | ✅ | ❌ | Go(工具) | evaluate_qa.py, print_qa_metrics.py |
| **LoCoMo-Plus** | ⚠️ 70% | ✅ | ❌ | - | evaluation_framework.py |
| **BEAM** | ✅ 100% | ✅ | ❌ | Julia(可选) | benchmark_generation.py, light_framework.py |

### 4.2 对 RollBall 的启示

**关键发现：**
1. **所有三个框架都是 Python 实现**，无 Rust 版本
2. **LongMemEval 最成熟**，直接可用（GitHub + HuggingFace）
3. **BEAM 代码最完整**，含完整生成和评估管道
4. **LoCoMo-Plus 最新**，但部分代码未开源

**RollBall 的 Rust 优势：**
- Grafeo（Rust 图数据库）可深度集成评估框架
- 可实现零成本抽象的性能评估工具
- 与 runtime/gateway 的紧密集成

**建议方案：**
```
Phase 2 - 评估基础设施
├─ Python 评估脚本层
│  └─ 复用 LongMemEval 的 evaluate_qa.py 框架
├─ Rust 后端层
│  ├─ 新增 rollball-eval crate
│  ├─ 实现内存评估的核心指标计算
│  └─ Grafeo 直接查询优化
└─ 分离关注点
   └─ Python 用于数据准备和结果分析
   └─ Rust 用于性能关键路径（大规模检索评估）
```

---

## 第五部分：三个框架对 RollBall 的详细适用性分析

### 5.1 LongMemEval 适用性评估

**优势：**
- ✅ 五个维度完全覆盖 RollBall 三层记忆的基础功能
- ✅ 500 个精心标注的问题，质量最高
- ✅ 属性受控的数据生成管道，易于扩展
- ✅ 完整开源，可直接复用代码
- ✅ 包含抽象（Abstention）维度，符合实际需求

**劣势：**
- ❌ 对话长度有限（最长 500 sessions），不适合超长测试
- ❌ 未涵盖隐式推理（LoCoMo-Plus 的认知记忆）
- ❌ 未测试工件感知检索（RollBall artifact_refs）
- ❌ 无衰减曲线分析（长期遗忘评估不足）

**适用场景：**
```
✅ Phase 1（基础记忆）评估
   ├─ 基础检索准确率测试
   ├─ 转折级/会话级召回率测试
   ├─ 知识更新处理测试
   └─ 时间推理测试

✅ Phase 2（程序记忆）初期验证
   ├─ ProceduralNode 触发准确率
   └─ Abstention 机制验证

❌ Phase 3（离线巩固）验证不足
   ├─ 超长对话性能衰减未测试
   └─ 隐式约束学习效果无评估
```

**建议：直接采用作为 Phase 1-2 的基础框架**

---

### 5.2 LoCoMo-Plus 适用性评估

**优势：**
- ✅ 认知记忆评估最符合 ProceduralNode + 隐式推理需求
- ✅ 约束一致性评分方法新颖，适合 RollBall 的隐式学习
- ✅ 识别了长度偏差问题，RollBall 应警惕
- ✅ LLM-as-Judge 框架灵活，可自定义评估标准

**劣势：**
- ❌ 核心代码部分未开源，复现困难
- ❌ 继承自原 LoCoMo 的 9K tokens 限制，不适合超长测试
- ❌ 数据集和评估脚本有待补充完善
- ❌ Cue-Trigger 不匹配的设计偏离 RollBall 常规对话场景

**适用场景：**
```
⚠️ Phase 2（程序记忆）的进阶评估
   ├─ ProceduralNode 的隐式约束学习评估
   ├─ 用户偏好隐式推断验证
   └─ 行为模式一致性评分

⚠️ Phase 3（自我认知）的部分验证
   ├─ AutobiographicalNode 的隐式推断
   └─ 用户关系推断准确率

❌ 基础检索评估（不是设计目标）
❌ 超长对话评估（仍在 9K tokens 范围）
```

**建议：作为 Phase 2-3 的补充框架，对齐约束一致性评分方法**

---

### 5.3 BEAM 适用性评估

**优势：**
- ✅ 最大对话长度（10M tokens），完全满足超长测试需求
- ✅ 10 个细粒度能力维度，覆盖 RollBall 的大部分检索场景
- ✅ 性能衰减曲线分析方法完整，适合评估 decay_score 有效性
- ✅ LIGHT 框架与 RollBall 三层架构天然对齐
- ✅ 代码和数据完整开源，可直接复用

**劣势：**
- ❌ 数据集规模仅 100 个对话（较小），但问题数 2000 个（足够）
- ❌ 对话合成方法复杂，自定义难度高
- ❌ 未专门测试工件检索（artifact-aware）
- ❌ LIGHT 框架的 Scratchpad 部分与 RollBall ProceduralNode 设计有差异

**适用场景：**
```
✅ Phase 3（离线巩固）的核心评估
   ├─ 超长对话性能衰减曲线
   ├─ 遗忘机制有效性验证
   ├─ 检索效率 vs 准确率 trade-off 分析
   └─ Token 效率评分 (TES)

✅ Phase 2 后期性能基准测试
   ├─ 相对于商用 LLM 的基准
   └─ 跨模型对比（Llama3 vs Qwen vs Claude）

✅ 工程验证
   ├─ 内存开销测试（1M+ tokens）
   ├─ 检索延迟评估
   └─ 索引大小分析
```

**建议：作为 Phase 3 的主要框架，用于长期记忆的稳定性和规模化评估**

---

## 第六部分：综合建议与分阶段实现路线

### 6.1 RollBall 应该借鉴的评估维度

**从 LongMemEval 借鉴（Phase 1-2）：**
1. **Information Extraction（IE）**：基础检索准确率
   - 实现：`hybrid_search` 的 Turn-Level Recall
   - 指标：准确率、精确率、F1 分数

2. **Multi-Session Reasoning（MR）**：多会话推理
   - 实现：`graph_expand` 的多跳推理评估
   - 指标：多会话准确率（需要 >=2 个源的问题）

3. **Temporal Reasoning（TR）**：时间推理
   - 实现：episode 时间戳过滤评估
   - 指标：时间关系理解准确率

4. **Knowledge Updates（KU）**：知识更新处理
   - 实现：冲突节点的 confidence/recency 更新
   - 指标：最新信息识别准确率

5. **Abstention（Abs）**：认知不确定性
   - 实现：检索分数阈值机制
   - 指标：正确拒绝回答的比例

**从 LoCoMo-Plus 借鉴（Phase 2-3）：**
1. **Constraint Consistency Score（CCS）**：约束一致性
   - 实现：ProceduralNode trigger_condition 遵守率
   - 指标：CCS = 遵守约束数 / 推断约束数

2. **Hidden Constraint Learning**：隐式约束学习
   - 实现：AutobiographicalNode 推断准确率
   - 指标：隐式推断成功率

**从 BEAM 借鉴（Phase 3）：**
1. **Accuracy@Length**：长度特定准确率
   - 实现：按对话长度分层评估
   - 指标：衰减曲线和 MDS（Memory Degradation Slope）

2. **Memory Robustness**：记忆鲁棒性
   - 实现：极长对话下的性能稳定性
   - 指标：在 1M/5M/10M tokens 下的准确率

3. **Retrieval Effectiveness Ratio（RER）**：检索效率
   - 实现：检索增强 vs 无增强的对比
   - 指标：性能提升倍数

---

### 6.2 是否值得复用评测数据集

| 数据集 | 推荐度 | 理由 | 复用方式 |
|--------|--------|------|---------|
| **LongMemEval** | ⭐⭐⭐⭐⭐ | 质量最高，维度最全，开源完整 | 直接使用或改编 |
| **LoCoMo** | ⭐⭐⭐ | 数据质量好，但长度有限；约束相关部分有价值 | 部分复用约束设计 |
| **BEAM** | ⭐⭐⭐⭐ | 超长对话测试必需；与 RollBall 架构对齐好 | 直接使用或扩展 |

**具体复用方案：**
```
Phase 1 评估数据集
├─ 基础数据：LongMemEval-S（~115K tokens）
├─ 扩展：创建 RollBall 特有的工件检索测试集
└─ 规模：500-1000 个问题

Phase 2 评估数据集
├─ 基础数据：LongMemEval-M（~500 sessions）
├─ 补充：从 LoCoMo-Plus 借鉴约束一致性测试
├─ 新增：ProceduralNode 隐式学习测试集
└─ 规模：1000+ 个问题

Phase 3 评估数据集
├─ 基础数据：BEAM（100 conversations, 2000 questions）
├─ 补充：100K-10M tokens 长度分层测试
├─ 新增：decay_score 衰减曲线验证集
└─ 规模：2000-5000 个问题（跨多个长度）
```

---

### 6.3 分阶段实现计划

#### **Phase 2 S1（记忆基础 - 当前）**

**立即实现的指标：**
1. Information Extraction Accuracy（IE 准确率）
   - 工作量：低（直接使用 LongMemEval 问题集）
   - 依赖：hybrid_search + RRF 排序
   - 目标：基准 75%+ 准确率

2. Turn-Level Recall（转折级召回）
   - 工作量：低
   - 实现：评估 episode 检索的召回率
   - 目标：基准 70%+

3. Session-Level Recall（会话级召回）
   - 工作量：中
   - 实现：需要关联 episode 到 session ID
   - 目标：基准 80%+

**临时跳过的指标：**
- ❌ Temporal Reasoning（需要时间戳稳定性）
- ❌ Knowledge Updates（需要冲突解决机制成熟）
- ❌ Abstention（需要 confidence_threshold 机制）

---

#### **Phase 2 S2（程序记忆与自我认知）**

**新增实现的指标：**
1. Multi-Session Reasoning（MR 准确率）
   - 工作量：高（需要 graph_expand）
   - 实现：通过 KnowledgeNode 关联的推理
   - 目标：基准 60%+（多跳推理通常难度高）

2. Temporal Reasoning（TR 准确率）
   - 工作量：中
   - 实现：episode 时间戳过滤 + 时序逻辑
   - 目标：基准 65%+

3. Knowledge Updates（KU 准确率）
   - 工作量：高
   - 实现：confidence 更新 + recency 权重
   - 目标：基准 70%+

4. Constraint Consistency Score（CCS）
   - 工作量：高（需要 ProceduralNode 成熟）
   - 实现：从 LoCoMo-Plus 移植约束评估
   - 目标：基准 65%+

**关键指标组合（性能基准）：**
```
Phase 2 验收标准（综合准确率）：
├─ Information Extraction：75%+ ✅ (Phase 1 已达成)
├─ Multi-Session Reasoning：60%+
├─ Temporal Reasoning：65%+
├─ Knowledge Updates：70%+
└─ 综合准确率（加权平均）：68%+
```

---

#### **Phase 3（离线巩固与持久化）**

**新增实现的指标：**
1. Abstention Accuracy（Abs 准确率）
   - 工作量：低-中
   - 实现：检索分数阈值 + "不确定" 信号
   - 目标：基准 75%+（高度依赖 confidence 校准）

2. Memory Degradation Slope（MDS）
   - 工作量：高（需要超长对话测试基础设施）
   - 实现：从 BEAM 移植长度分层评估
   - 测试长度：100K → 500K → 1M tokens
   - 目标：MDS < 0.1（衰减不超过 10%）

3. Retrieval Effectiveness Ratio（RER）
   - 工作量：中
   - 实现：+/-检索的准确率对比
   - 目标：RER > 1.5（检索至少帮助 50% 性能提升）

4. Token Efficiency Score（TES）
   - 工作量：低
   - 实现：准确率 / 上下文 token 消耗
   - 目标：基准 0.05+（每 20 tokens 获得 1% 准确率提升）

5. Episodic-Semantic Bridge Quality（新增）
   - 工作量：高（RollBall 特有）
   - 实现：source_episode 反向查询的有效性
   - 目标：基准 70%+（跨层检索的完成度）

**关键指标组合（长期记忆基准）：**
```
Phase 3 验收标准：
├─ Phase 2 维护：综合准确率 ≥68%（在 9K tokens）
├─ Abstention：75%+
├─ Long-Context Accuracy@500K：降幅 <20%
├─ Long-Context Accuracy@1M：降幅 <35%
├─ MDS：<0.1
├─ RER：>1.5
├─ TES：>0.05
└─ Episodic-Semantic Bridge：70%+
```

---

## 第七部分：评估框架实现建议

### 7.1 核心指标计算公式库（可复用）

```python
# Python 评估框架框架（Phase 2 开始实现）

class RollBallEvaluator:
    """RollBall 三层记忆的通用评估器"""
    
    def turn_level_recall(self, predicted_turns, gold_turns):
        """转折级召回率（LongMemEval）"""
        return len(set(predicted_turns) & set(gold_turns)) / len(gold_turns)
    
    def session_level_recall(self, predicted_sessions, gold_sessions):
        """会话级召回率（LongMemEval）"""
        return len(set(predicted_sessions) & set(gold_sessions)) / len(gold_sessions)
    
    def constraint_consistency_score(self, inferred_constraints, response_satisfies):
        """约束一致性评分（LoCoMo-Plus）"""
        return sum(response_satisfies) / len(inferred_constraints)
    
    def accuracy_at_length(self, dialogues_by_length, qa_pairs):
        """长度特定准确率（BEAM）"""
        results = {}
        for length, dialogues in dialogues_by_length.items():
            correct = sum(
                self.evaluate_qa(dialogue, q, a)
                for dialogue, (q, a) in zip(dialogues, qa_pairs)
            )
            results[length] = correct / len(qa_pairs)
        return results
    
    def memory_degradation_slope(self, accuracy_curve):
        """记忆衰减斜率（BEAM）"""
        lengths = sorted(accuracy_curve.keys())
        log_lengths = [math.log10(l) for l in lengths]
        accuracies = [accuracy_curve[l] for l in lengths]
        
        # 线性回归计算斜率
        slope = np.polyfit(log_lengths, accuracies, 1)[0]
        return slope
    
    def retrieval_effectiveness_ratio(self, with_retrieval_acc, no_retrieval_acc):
        """检索效率比（BEAM）"""
        return with_retrieval_acc / no_retrieval_acc if no_retrieval_acc > 0 else 1.0
```

### 7.2 分层测试数据生成（Phase 2-3）

```python
# 基于属性受控生成（从 LongMemEval 移植）

def generate_rollball_benchmark(phase, num_questions=500):
    """
    生成 RollBall 特定的评估基准
    
    phase: 1 | 2 | 3（对应 Phase 1 | Phase 2 | Phase 3）
    """
    
    if phase == 1:
        # 基础检索测试（LongMemEval-S 风格）
        return {
            'ie_questions': generate_ie_questions(100),  # 信息提取
            'mr_questions': generate_mr_questions(100),  # 多会话推理
            'tr_questions': generate_tr_questions(100),  # 时间推理
            'ku_questions': generate_ku_questions(100),  # 知识更新
            'abs_questions': generate_abs_questions(100), # 抽象
        }
    
    elif phase == 2:
        # 增强推理测试（LoCoMo-Plus 风格）
        return {
            'constraint_questions': generate_constraint_questions(200),
            'procedural_questions': generate_procedural_questions(150),
            'autobiographical_questions': generate_autobiographical_questions(150),
        }
    
    elif phase == 3:
        # 超长对话测试（BEAM 风格）
        return {
            'long_dialogue_100k': generate_long_dialogue(100_000),
            'long_dialogue_500k': generate_long_dialogue(500_000),
            'long_dialogue_1m': generate_long_dialogue(1_000_000),
            'degradation_questions': generate_degradation_questions(500),
        }
```

---

## 第八部分：关键决策总结

### 8.1 三个框架的选择决策

| 决策项 | 推荐 | 理由 |
|--------|------|------|
| Phase 1-2 基础框架 | **LongMemEval** | 最成熟、最完整、最易集成 |
| Phase 2 隐式学习补充 | **LoCoMo-Plus** | 约束一致性评分方法新颖 |
| Phase 3 长期评估框架 | **BEAM** | 超长对话测试完全满足需求 |
| 直接复用数据集 | **LongMemEval** | 质量最高，500 个问题足够 |
| 采用的衰减评估方法 | **BEAM MDS** | 更接近实际应用需求 |

### 8.2 RollBall 的评估框架架构

```
┌─────────────────────────────────────────────────────┐
│ RollBall Evaluation Framework (Phase 2+)            │
├─────────────────────────────────────────────────────┤
│                                                      │
│ ┌─ Phase 1 评估（LongMemEval 框架）─────────┐      │
│ │ ├─ IE: Information Extraction              │      │
│ │ ├─ MR: Multi-Session Reasoning             │      │
│ │ ├─ TR: Temporal Reasoning                  │      │
│ │ ├─ KU: Knowledge Updates                   │      │
│ │ └─ Abs: Abstention                         │      │
│ └─────────────────────────────────────────────┘      │
│                                                      │
│ ┌─ Phase 2 补充（LoCoMo-Plus + LongMemEval）──┐    │
│ │ ├─ CCS: Constraint Consistency Score        │    │
│ │ ├─ ProceduralNode 隐式学习                  │    │
│ │ ├─ AutobiographicalNode 推断准确率          │    │
│ │ └─ 工件检索准确率（RollBall 特有）          │    │
│ └──────────────────────────────────────────────┘    │
│                                                      │
│ ┌─ Phase 3 扩展（BEAM 框架）─────────────────┐     │
│ │ ├─ Accuracy@Length（多长度层级）           │     │
│ │ ├─ MDS: Memory Degradation Slope           │     │
│ │ ├─ RER: Retrieval Effectiveness Ratio      │     │
│ │ ├─ TES: Token Efficiency Score             │     │
│ │ └─ 衰减曲线分析（decay_score 验证）        │     │
│ └──────────────────────────────────────────────┘    │
│                                                      │
│ 底层实现：                                         │
│ ├─ Rust: rollball-eval crate（性能关键路径）      │
│ ├─ Python: 数据生成和结果分析脚本               │
│ └─ Grafeo: 直接查询优化                         │
│                                                      │
└─────────────────────────────────────────────────────┘
```

### 8.3 工程化建议

**实现优先级：**
```
🔴 高优先级（Phase 2 S1）
├─ Turn-Level Recall（转折级召回）
├─ Information Extraction Accuracy（IE 准确率）
└─ Session-Level Recall（会话级召回）

🟠 中优先级（Phase 2 S2）
├─ Multi-Session Reasoning（MR 准确率）
├─ Temporal Reasoning（TR 准确率）
├─ Knowledge Updates（KU 准确率）
└─ Constraint Consistency Score（CCS）

🟡 低优先级（Phase 3 之前）
├─ Abstention（需要阈值机制稳定）
├─ Memory Degradation Slope（超长对话基础设施）
├─ Retrieval Effectiveness Ratio（性能基准建立）
└─ Token Efficiency Score（工程优化）
```

**关键里程碑：**
```
Week 1-2: 集成 LongMemEval 评估脚本
Week 3-4: 实现 Phase 1 基础指标
Week 5-6: 集成 LoCoMo-Plus 约束评分
Week 7-8: 建立超长对话测试基础设施
Week 9-10: 实现 BEAM 衰减曲线分析
Week 11-12: 性能基准建立和优化
```

---

## 第九部分：风险与注意事项

### 9.1 应注意的陷阱

1. **长度偏差（Length Bias）**
   - 问题：模型倾向于在长对话中输出更长的文本以隐藏不确定性
   - LoCoMo-Plus 的发现
   - 解决：生成长度标准化的评估指标

2. **Accuracy 不能完全反映记忆质量**
   - 问题：高准确率可能源于 LLM 的参数知识而非记忆系统
   - 解决：使用"Oracle 检索"版本（仅包含答案所在会话）进行对比

3. **数据泄露（Data Contamination）**
   - 问题：LLM 可能在预训练中见过评估数据
   - 解决：创建 RollBall 特定的新数据集，不依赖公开基准

4. **评估成本（Evaluation Cost）**
   - LongMemEval：每个问题需要 1 次 LLM 调用（GPT-4o），成本高
   - 解决：建立本地 LLM 评估器或混合评估策略

### 9.2 RollBall 特有的评估需求

1. **Grafeo 查询性能评估**
   - 指标：查询延迟、内存占用、索引效率
   - 工具：集成 Grafeo 的性能分析工具

2. **多 Agent 场景评估**
   - 问题：跨 Agent Intent 查询的准确率
   - 测试：多个 Agent 共享信息的场景

3. **打包分享时的隐私过滤**
   - 问题：PrivacyLevel 是否正确剥离 Personal/Sensitive 节点
   - 测试：打包前后的节点数变化

4. **中文支持**
   - 问题：向量检索和全文检索在中文上的性能
   - 解决：创建中英双语评估数据集

---

## 第十部分：最终建议总结

### 综合建议

**三个框架的核心价值与 RollBall 映射**

| 框架 | 核心价值 | 与 RollBall 的映射 |
|------|---------|------------------|
| **LongMemEval** (ICLR 2025) | 5 维能力评估：信息提取(IE)、跨会话推理(MR)、时序推理(TR)、知识更新(KU)、拒绝回答(Abs) | KU 直接对应冲突处理，Abs 对应"不知道就说不知道" |
| **LoCoMo-Plus** | 约束一致性评分(CCS)——评估隐式偏好推断准确率 | 直接对应 Preference 和 ProceduralNode 的提取质量 |
| **BEAM** (ICLR 2026) | 10 维能力 + 性能随对话长度衰减曲线 | 矛盾检测、偏好学习、知识演化与三层架构高度对应；衰减曲线可验证 λ 参数 |

### 分阶段实施方案

**Phase 2：借鉴 LongMemEval 5 维评估 + 可观测指标**

```
Phase 2 评估体系：

1. 借鉴 LongMemEval 5 维能力定义，作为 RollBall 记忆系统的评估标准
   - IE（信息提取）→ memory_store 是否正确捕获用户陈述
   - MR（跨会话推理）→ 跨 episode 的检索关联能力
   - TR（时序推理）→ 时间相关记忆的正确排序
   - KU（知识更新）→ 冲突处理的正确率（直接验证 §6.9 设计）
   - Abs（拒绝回答）→ 记忆缺失时不编造的能力

2. 运行时可观测指标
   - 节点统计：Active / Dormant / Pending 分布
   - 检索统计：avg_latency / hit_rate / skip_rate
   - 冲突统计：pending / auto_resolved / user_confirmed

3. 集成测试中的性能 SLA 断言
   - hybrid_search: P99 < 100ms (1K nodes)
   - memory_store: < 50ms
   - embedding 生成: < 200ms
```

**Phase 3：复用 BEAM 数据集 + 衰减参数验证**

```
Phase 3 扩展：

1. 复用 BEAM 的多长度评测数据，验证记忆随对话长度的衰减曲线
   - 用 BEAM 的 100K/500K/1M 数据跑 RollBall 的检索管线
   - 绘制 Accuracy@Length 曲线
   - 与 BEAM 论文中的基线对比

2. 用 LoCoMo-Plus 的 CCS 评估隐式偏好提取
   - 验证离线巩固是否正确推断 Preference/Procedural

3. 基于真实数据校准衰减参数
   - 固定 λ=0.03 运行 3 个月积累数据
   - 用 BEAM 的 Memory Degradation Slope 指标验证
   - 如有显著偏差再调参
```

**衰减参数策略：参数外置 + 可观测 + 运行时可调**

```
衰减参数策略：

1. 所有衰减参数通过 manifest 可配置（符合 RXT-03 配置外置准则）
   [memory.decay]
   lambda = 0.03
   floor = 0.05
   boost_cap = 0.5
   access_per_hit = 0.1
   dormant_threshold = 0.3

2. 衰减扫描时输出可观测指标
   - total_active / total_dormant / total_purge_candidate
   - 平均 decay_score（按节点类型分组）
   - Dormant 恢复为 Active 的次数（衰减过快的信号）

3. Phase 3 用真实数据 + BEAM 衰减曲线验证
   - Dormant→Active 恢复率 > 20% → λ 太大
   - Active 持续增长、Dormant 转化率 < 5% → λ 太小或 BOOST_CAP 太高
```

### 总结表

| 维度 | Phase 2 | Phase 3 |
|------|---------|--------|
| 评估维度 | 借鉴 LongMemEval 5 维定义作为标准 | 扩展到 BEAM 10 维 |
| 评测数据 | 集成测试用例覆盖 5 维 | 复用 BEAM/LoCoMo-Plus 开源数据集 |
| 衰减参数 | 参数外置 + 可观测指标 | BEAM 衰减曲线 + 真实数据校准 |
| 检索质量 | 隐式反馈（hit_rate 等） | LongMemEval 标注对 + LLM-as-judge |
| 性能基准 | SLA 断言 | BEAM Retrieval Effectiveness Ratio |

