import unittest
import verify_final_design as v


class FinalVerifierValidationTests(unittest.TestCase):
    def setUp(self):
        self.history=[{'role':'user','content':'两个字段必须独立'}]
        self.draft='把两个字段合并。'
        self.finding={'draft_quote':'两个字段合并','kind':'contradiction','history_index':0,'history_quote':'两个字段必须独立'}

    def test_requires_existing_draft_and_source_quotes(self):
        good={'findings':[self.finding],'final_answer':'保留两个字段'}
        self.assertEqual(v.validate(good,self.history,self.draft),good)
        for changes in [{'draft_quote':'编造的原话'},{'history_quote':'编造的约束'},{'history_index':True},{'history_index':2}]:
            with self.assertRaises(ValueError):
                v.validate({**good,'findings':[{**self.finding,**changes}]},self.history,self.draft)

    def test_no_findings_must_not_silently_rewrite_a_correct_answer(self):
        v.validate({'findings':[],'final_answer':self.draft},self.history,self.draft)
        with self.assertRaisesRegex(ValueError,'Unreported rewrite'):
            v.validate({'findings':[],'final_answer':'偷偷改掉'},self.history,self.draft)

    def test_absent_evidence_cannot_be_presented_as_a_grounded_contradiction(self):
        finding={**self.finding,'history_index':None,'history_quote':''}
        with self.assertRaisesRegex(ValueError,'Ungrounded contradiction'):
            v.validate({'findings':[finding],'final_answer':'修改'},self.history,self.draft)
        finding['kind']='unsupported_fact'
        v.validate({'findings':[finding],'final_answer':'标记未确认'},self.history,self.draft)


if __name__=='__main__':unittest.main()
