import unittest
from verify_minimal_edits import apply_edits


class MinimalEditTests(unittest.TestCase):
    def edit(self,old,new):
        return {'old':old,'new':new,'kind':'unsupported_fact','history_index':None,'history_quote':''}

    def test_all_offsets_refer_to_original_unicode_draft(self):
        edits=[self.edit('甲','改成长一些的甲'),self.edit('丙','末尾')]
        self.assertEqual(apply_edits({'edits':edits},[],'甲乙丙'),'改成长一些的甲乙末尾')
        self.assertEqual(apply_edits({'edits':edits[::-1]},[],'甲乙丙'),'改成长一些的甲乙末尾')

    def test_ambiguous_missing_or_overlapping_excerpts_are_rejected(self):
        for draft,edits in [('甲甲',[self.edit('甲','新')]),('甲',[self.edit('乙','新')]),('甲乙丙',[self.edit('甲乙','新'),self.edit('乙丙','另')])]:
            with self.assertRaises(ValueError):apply_edits({'edits':edits},[],draft)

    def test_no_edits_preserves_all_bytes(self):
        draft='  原文\n`代码`\n'
        self.assertEqual(apply_edits({'edits':[]},[],draft),draft)

    def test_partial_valid_prefix_is_not_applied_when_later_edit_invalid(self):
        value={'edits':[self.edit('甲','已改'),self.edit('不存在','失败')]}
        draft='甲乙'
        with self.assertRaises(ValueError):apply_edits(value,[],draft)
        self.assertEqual(draft,'甲乙')

    def test_contradictions_require_exact_history_evidence_and_real_index(self):
        edit={**self.edit('甲','乙'),'kind':'contradiction'}
        history=[{'role':'user','content':'保留乙'}]
        for index,quote in [(None,''),(True,'保留乙'),(1,'保留乙'),(0,'编造')]:
            with self.assertRaises(ValueError):apply_edits({'edits':[{**edit,'history_index':index,'history_quote':quote}]},history,'甲')
        valid={**edit,'history_index':0,'history_quote':'保留乙'}
        self.assertEqual(apply_edits({'edits':[valid]},history,'甲'),'乙')


if __name__=='__main__':unittest.main()
