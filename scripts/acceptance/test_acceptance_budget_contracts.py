#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
"""Inverse controls for impact paging and the stable doctor census check."""
import copy
import unittest
from magic_repro import grade_doctor_census, PASS, FAIL, UNREADABLE
from response_budget_elisions import grade_impact_pages
from trace_pages import TraceAnswer, restore_compacted


class AcceptanceBudgetContracts(unittest.TestCase):
    def test_advertised_compaction_restores_exact_safety_readings(self):
        compacted = [
            {'path': 'source_derivation', 'same_as': '_kin.source_derivation'},
            {'path': 'negative.advice', 'restates': ['negative.subject', '_kin.graph_as_of',
                                                   'negative.trust_reason']},
        ]
        answer = {'_kin': {'source_derivation': {'identity': 'current'},
                           'graph_as_of': {'generation': 2}},
                  'negative': {'subject': 'per-entity verdicts', 'trust_reason': 'current inputs',
                               'advice': '{negative.subject} at {_kin.graph_as_of}; {negative.trust_reason}'}}
        restored = restore_compacted(copy.deepcopy(answer), compacted)
        self.assertEqual(restored['source_derivation'], answer['_kin']['source_derivation'])
        self.assertEqual(restored['negative']['advice'],
                         'per-entity verdicts at {"generation":2}; current inputs')
        self.assertEqual(answer['negative']['advice'],
                         '{negative.subject} at {_kin.graph_as_of}; {negative.trust_reason}')
        clause_answer = {
            '_kin': {'source_derivation': {'identity': 'current'},
                     'graph_as_of': {'generation': 2}},
            'negative': {
                'subject': 'all direct call and non-call references to Target',
                'trust_reason': 'local_binding_unproven: prior-local binding evidence could not be validated; reference_completion_unmeasured: non-call completion is unmeasured',
                'advice': '{negative.subject} at {_kin.graph_as_of}; local_binding_unproven; reference_completion_unmeasured ({negative.trust_reason})',
            },
        }
        clause_restored = restore_compacted(copy.deepcopy(clause_answer), compacted)
        self.assertEqual(
            clause_restored['negative']['advice'],
            'all direct call and non-call references to Target at {"generation":2}; '
            + clause_answer['negative']['trust_reason'],
        )
        for changed in (
            dict(answer, negative={'advice': '{negative.subject}'}),
            dict(answer, negative=dict(answer['negative'], advice='unadvertised content')),
        ):
            with self.assertRaises(ValueError):
                restore_compacted(copy.deepcopy(changed), compacted)
        with self.assertRaisesRegex(ValueError, 'twice'):
            restore_compacted(copy.deepcopy(answer), compacted + [compacted[-1]])
        with self.assertRaisesRegex(ValueError, 'unknown compacted metadata'):
            restore_compacted(copy.deepcopy(answer), [{'path': 'negative.advice', 'unknown': True}])

    def test_census_uses_stable_id_and_exact_forced_loss(self):
        expected = 'Calls slipped 11 to 10'
        row = {'id': 'relation_census', 'label': 'Relationship counts', 'status': 'stale',
               'detail': expected + '; entity count held at 18'}
        self.assertEqual(grade_doctor_census({'checks': [row]}, expected)[0], PASS)
        row['label'] = 'A future display label'
        self.assertEqual(grade_doctor_census({'checks': [row]}, expected)[0], PASS)
        for report in ({}, {'checks': []}, {'checks': [row, row]}, {'checks': {}}):
            self.assertEqual(grade_doctor_census(report, expected)[0], UNREADABLE)
        for status in ('healthy', 'pending', 'unsupported', 'missing', 'misconfigured', 'degraded'):
            changed = dict(row, status=status)
            self.assertEqual(grade_doctor_census({'checks': [changed]}, expected)[0], FAIL)
        self.assertEqual(grade_doctor_census({'checks': [dict(row, status='new')]}, expected)[0], UNREADABLE)
        self.assertEqual(grade_doctor_census({'checks': [dict(row, detail=None)]}, expected)[0], UNREADABLE)
        self.assertEqual(grade_doctor_census({'checks': [dict(row, detail='Calls slipped 12 to 10; entity count held')]}, expected)[0], FAIL)
        self.assertEqual(grade_doctor_census({'checks': [dict(row, detail=expected)]}, expected)[0], FAIL)

    def test_impact_grader_rejects_lost_fields_and_changed_safety(self):
        whole = TraceAnswer({'affected_callers': [{'id': 'caller', 'body': 'semantic body'}],
                             'entity_impacts': [{'entity_id': 'focal', 'consumer_count': 1}],
                             'changed_ids': ['focal'],
                             '_kin': {'verdict': {'state': 'inconclusive'},
                                      'completeness': {'status': 'partial'}},
                             'negative': {'safe_to_conclude_absent': False}})
        paged = TraceAnswer(copy.deepcopy(whole))
        paged.page_bytes = (1800, 1700)
        self.assertEqual(grade_impact_pages(whole, paged), [])
        for key in ('affected_callers', 'entity_impacts', 'changed_ids'):
            cut = TraceAnswer(copy.deepcopy(paged)); cut.page_bytes = paged.page_bytes
            cut.pop(key)
            self.assertTrue(grade_impact_pages(whole, cut), key)
        cut = TraceAnswer(copy.deepcopy(paged)); cut.page_bytes = paged.page_bytes
        cut['affected_callers'][0].pop('body')
        self.assertTrue(grade_impact_pages(whole, cut))
        for key in ('verdict', 'completeness'):
            cut = TraceAnswer(copy.deepcopy(paged)); cut.page_bytes = paged.page_bytes
            cut['_kin'].pop(key)
            self.assertTrue(grade_impact_pages(whole, cut), key)
        cut = TraceAnswer(copy.deepcopy(paged)); cut.page_bytes = paged.page_bytes
        cut['negative']['safe_to_conclude_absent'] = True
        self.assertTrue(grade_impact_pages(whole, cut))
        paged.page_bytes = (1700,)
        self.assertTrue(grade_impact_pages(whole, paged))


if __name__ == '__main__':
    unittest.main()
